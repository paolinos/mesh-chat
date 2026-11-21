use std::time::Duration;

use libp2p::{
    gossipsub, identify, identity::Keypair, mdns, request_response,
    swarm::{NetworkBehaviour, StreamProtocol},
};

use super::protocol::{ChatRequest, ChatResponse, CHAT_PROTOCOL, PRESENCE_TOPIC};

pub type ChatRequestResponse = request_response::cbor::Behaviour<ChatRequest, ChatResponse>;

#[derive(NetworkBehaviour)]
pub struct ChatBehaviour {
    pub mdns: mdns::tokio::Behaviour,
    pub identify: identify::Behaviour,
    pub request_response: ChatRequestResponse,
    pub gossipsub: gossipsub::Behaviour,
}

pub fn new_behaviour(keypair: &Keypair) -> Result<ChatBehaviour, String> {
    let local_peer_id = keypair.public().to_peer_id();

    let mdns = mdns::tokio::Behaviour::new(mdns::Config::default(), local_peer_id)
        .map_err(|e| format!("failed to start mDNS: {e}"))?;

    let identify = identify::Behaviour::new(identify::Config::new(
        CHAT_PROTOCOL.to_string(),
        keypair.public(),
    ));

    let request_response = ChatRequestResponse::new(
        [(
            StreamProtocol::new(CHAT_PROTOCOL),
            request_response::ProtocolSupport::Full,
        )],
        request_response::Config::default().with_request_timeout(Duration::from_secs(10)),
    );

    let gossipsub_config = gossipsub::ConfigBuilder::default()
        .build()
        .map_err(|e| format!("invalid gossipsub config: {e}"))?;
    let mut gossipsub = gossipsub::Behaviour::new(
        gossipsub::MessageAuthenticity::Signed(keypair.clone()),
        gossipsub_config,
    )
    .map_err(|e| format!("failed to start gossipsub: {e}"))?;
    gossipsub
        .subscribe(&presence_topic())
        .map_err(|e| format!("failed to subscribe to presence topic: {e}"))?;

    Ok(ChatBehaviour {
        mdns,
        identify,
        request_response,
        gossipsub,
    })
}

pub fn presence_topic() -> gossipsub::IdentTopic {
    gossipsub::IdentTopic::new(PRESENCE_TOPIC)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presence_topic_uses_the_shared_constant() {
        assert_eq!(presence_topic().to_string(), PRESENCE_TOPIC);
    }

    #[test]
    fn request_response_behaviour_can_be_built() {
        // No sockets are opened here; construction only validates the protocol list.
        let _ = ChatRequestResponse::new(
            [(
                StreamProtocol::new(CHAT_PROTOCOL),
                request_response::ProtocolSupport::Full,
            )],
            request_response::Config::default(),
        );
    }
}
