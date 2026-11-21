use std::collections::HashSet;

use libp2p::{Multiaddr, PeerId};

/// Filter out our own addresses and duplicate addresses reported by mDNS.
pub fn discovered_peers(
    peers: &[(PeerId, Multiaddr)],
    local: &PeerId,
) -> Vec<(PeerId, Multiaddr)> {
    let mut seen = HashSet::new();
    let mut found = Vec::new();
    for (peer, address) in peers {
        if peer == local {
            continue;
        }
        if seen.insert(address.clone()) {
            found.push((*peer, address.clone()));
        }
    }
    found
}

/// Only the peer with the lexicographically smaller id dials.
///
/// Two peers started at the same time otherwise dial each other simultaneously, which can make the
/// Noise handshake fail with `InvalidData: Input` (a known libp2p issue). One initiator is enough:
/// the other side accepts the inbound connection.
pub fn should_initiate_dial(local: &PeerId, remote: &PeerId) -> bool {
    local.to_string() < remote.to_string()
}

/// Human-readable label that stays unambiguous when two users pick the same name.
/// All PeerIds start with `12D3...`, so the *last* four characters are the useful suffix.
pub fn display_name(username: &str, peer_id: &PeerId) -> String {
    format!("{}·{}", username, short_peer_id(peer_id))
}

pub fn short_peer_id(peer_id: &PeerId) -> String {
    let encoded = peer_id.to_string();
    let skip = encoded.chars().count().saturating_sub(4);
    encoded.chars().skip(skip).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(port: u16) -> Multiaddr {
        format!("/ip4/127.0.0.1/tcp/{port}").parse().unwrap()
    }

    #[test]
    fn dedupes_addresses_and_skips_self() {
        let local = PeerId::random();
        let other = PeerId::random();
        let peers = vec![
            (local, addr(1000)),
            (other, addr(2000)),
            (other, addr(2000)),
            (other, addr(3000)),
        ];
        assert_eq!(
            discovered_peers(&peers, &local),
            vec![(other, addr(2000)), (other, addr(3000))]
        );
    }

    #[test]
    fn exactly_one_peer_initiates_a_dial() {
        let a = PeerId::random();
        let b = PeerId::random();
        assert_ne!(should_initiate_dial(&a, &b), should_initiate_dial(&b, &a));
    }

    #[test]
    fn display_name_contains_name_and_short_id() {
        let peer = PeerId::random();
        let name = display_name("alice", &peer);
        assert!(name.starts_with("alice·"));
        assert_eq!(name, format!("alice·{}", short_peer_id(&peer)));
        assert_eq!(short_peer_id(&peer).len(), 4);
    }
}
