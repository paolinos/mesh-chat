use serde::{Deserialize, Serialize};

/// Versioned protocol string used by both identify and request-response.
pub const CHAT_PROTOCOL: &str = "/p2p-chat/1.0.0";

/// Gossipsub topic used for LAN-wide presence announcements.
pub const PRESENCE_TOPIC: &str = "chat/presence";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChatRequest {
    Hello {
        username: String,
    },
    Chat(ChatMessage),
    // Reserved for the private groups introduced in phase 2.
    GroupInvite {
        group_id: String,
        group_name: String,
        invited_by: String,
    },
    MemberAdded {
        group_id: String,
        peer_id: String,
        username: String,
        invited_by: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChatResponse {
    HelloAck {
        username: String,
    },
    /// `outbox_id` is `None` in phase 1 (no database yet) and `Some` from phase 3.
    Ack {
        outbox_id: Option<u64>,
        ok: bool,
        error: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub id: String,
    pub from_peer: String,
    pub from_username: String,
    /// `#general`, `dm:<peer_id>` or `<group_uuid>`.
    pub channel: String,
    pub body: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Presence {
    pub peer_id: String,
    pub username: String,
    pub online: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_are_stable() {
        assert_eq!(CHAT_PROTOCOL, "/p2p-chat/1.0.0");
        assert_eq!(PRESENCE_TOPIC, "chat/presence");
    }

    #[test]
    fn chat_request_round_trip() {
        let request = ChatRequest::Chat(ChatMessage {
            id: "m-1".to_string(),
            from_peer: "peer".to_string(),
            from_username: "alice".to_string(),
            channel: "#general".to_string(),
            body: "hello\nworld".to_string(),
            created_at: 42,
        });
        let bytes = serde_json::to_vec(&request).unwrap();
        assert_eq!(serde_json::from_slice::<ChatRequest>(&bytes).unwrap(), request);
    }

    #[test]
    fn group_requests_round_trip() {
        let invite = ChatRequest::GroupInvite {
            group_id: "g-1".to_string(),
            group_name: "team".to_string(),
            invited_by: "peer-a".to_string(),
        };
        let bytes = serde_json::to_vec(&invite).unwrap();
        assert_eq!(serde_json::from_slice::<ChatRequest>(&bytes).unwrap(), invite);

        let member = ChatRequest::MemberAdded {
            group_id: "g-1".to_string(),
            peer_id: "peer-b".to_string(),
            username: "bob".to_string(),
            invited_by: "peer-a".to_string(),
        };
        let bytes = serde_json::to_vec(&member).unwrap();
        assert_eq!(serde_json::from_slice::<ChatRequest>(&bytes).unwrap(), member);
    }

    #[test]
    fn responses_and_presence_round_trip() {
        let ack = ChatResponse::Ack {
            outbox_id: None,
            ok: true,
            error: None,
        };
        let bytes = serde_json::to_vec(&ack).unwrap();
        assert_eq!(serde_json::from_slice::<ChatResponse>(&bytes).unwrap(), ack);

        let presence = Presence {
            peer_id: "peer".to_string(),
            username: "bob".to_string(),
            online: false,
        };
        let bytes = serde_json::to_vec(&presence).unwrap();
        assert_eq!(serde_json::from_slice::<Presence>(&bytes).unwrap(), presence);
    }
}
