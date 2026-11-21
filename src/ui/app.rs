use std::{
    collections::{HashMap, HashSet},
    io,
    time::{SystemTime, UNIX_EPOCH},
};

use crossterm::{
    cursor::{Hide, Show},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use indexmap::IndexMap;
use libp2p::PeerId;
use ratatui::{backend::CrosstermBackend, style::Color, Terminal};
use uuid::Uuid;

use crate::{database::History, network::{events::display_name, protocol::ChatMessage}};

pub const GENERAL: &str = "#general";
/// Keep the TUI bounded in phase 2; phase 3 will read full history from SQLite.
pub const MAX_LINES_PER_CONVERSATION: usize = 1000;

/// Fixed color for messages written by the local user.
pub const OWN_COLOR: Color = Color::Green;

#[derive(Debug, Clone, PartialEq)]
pub struct Peer {
    pub username: String,
    pub online: bool,
    /// Derived once from the PeerId so it is identical in every channel.
    pub color: Color,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatLine {
    Message {
        id: String,
        author: String,
        peer: Option<PeerId>,
        body: String,
        created_at: i64,
        mine: bool,
    },
    System(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub id: String,
    pub name: String,
    pub members: HashSet<PeerId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Selection {
    #[default]
    None,
    Channels,
    Groups,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    NotFound(String),
    Ambiguous(String, Vec<String>),
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolveError::NotFound(name) => write!(f, "unknown user or group: {name}"),
            ResolveError::Ambiguous(name, matches) => {
                write!(f, "'{name}' is ambiguous: {}", matches.join(", "))
            }
        }
    }
}

/// Restore the terminal on every exit path, including errors and panics in the loop.
pub struct TerminalGuard;

impl TerminalGuard {
    pub fn new() -> io::Result<Self> {
        enable_raw_mode()?;
        let guard = TerminalGuard;
        if let Err(error) = execute!(io::stdout(), EnterAlternateScreen, Hide) {
            guard.restore();
            return Err(error);
        }
        Ok(guard)
    }

    fn restore(&self) {
        let _ = execute!(io::stdout(), Show, LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

pub type TerminalHandle = Terminal<CrosstermBackend<io::Stdout>>;

pub fn terminal() -> io::Result<TerminalHandle> {
    Terminal::new(CrosstermBackend::new(io::stdout()))
}

/// Stable FNV-1a hash of the PeerId -> hue -> RGB. No randomness, no state.
pub fn color_for(peer_id: &PeerId) -> Color {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in peer_id.to_string().bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    let (r, g, b) = hsl_to_rgb((hash % 360) as f64, 0.65, 0.62);
    Color::Rgb(r, g, b)
}

fn hsl_to_rgb(hue: f64, saturation: f64, lightness: f64) -> (u8, u8, u8) {
    let chroma = (1.0 - (2.0 * lightness - 1.0).abs()) * saturation;
    let x = chroma * (1.0 - ((hue / 60.0) % 2.0 - 1.0).abs());
    let m = lightness - chroma / 2.0;
    let (r, g, b) = match hue as u32 / 60 {
        0 => (chroma, x, 0.0),
        1 => (x, chroma, 0.0),
        2 => (0.0, chroma, x),
        3 => (0.0, x, chroma),
        4 => (x, 0.0, chroma),
        _ => (chroma, 0.0, x),
    };
    (
        ((r + m) * 255.0).round() as u8,
        ((g + m) * 255.0).round() as u8,
        ((b + m) * 255.0).round() as u8,
    )
}

pub fn make_message(
    me: &PeerId,
    username: &str,
    channel: String,
    body: String,
) -> ChatMessage {
    ChatMessage {
        id: Uuid::new_v4().to_string(),
        from_peer: me.to_string(),
        from_username: username.to_string(),
        channel,
        body,
        created_at: now_millis(),
        outbox_id: None,
    }
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

pub struct App<'a> {
    pub me: PeerId,
    pub username: String,
    pub peers: HashMap<PeerId, Peer>,
    pub conversations: IndexMap<String, Vec<ChatLine>>,
    pub groups: Vec<Group>,
    pub active_conversation: String,
    pub selection: Selection,
    pub channel_selection: usize,
    pub group_selection: usize,
    pub textarea: tui_textarea::TextArea<'a>,
    /// message id -> peers whose ACK has not arrived yet.
    pending: HashMap<String, HashSet<PeerId>>,
    seen_messages: HashSet<String>,
}

impl<'a> App<'a> {
    pub fn new(me: PeerId, username: String) -> App<'static> {
        let mut textarea = tui_textarea::TextArea::default();
        textarea.set_placeholder_text("Type a message; Shift+Enter inserts a newline");
        let mut conversations = IndexMap::new();
        conversations.insert(GENERAL.to_string(), Vec::new());
        App {
            me,
            username,
            peers: HashMap::new(),
            conversations,
            groups: Vec::new(),
            active_conversation: GENERAL.to_string(),
            selection: Selection::None,
            channel_selection: 0,
            group_selection: 0,
            textarea,
            pending: HashMap::new(),
            seen_messages: HashSet::new(),
        }
    }

    /// Fill the TUI from SQLite on startup without re-broadcasting presence from old messages.
    pub fn apply_history(&mut self, history: History) {
        for peer in history.peers {
            if peer.is_local {
                continue;
            }
            let Ok(peer_id) = peer.peer_id.parse::<PeerId>() else {
                continue;
            };
            // Presence is live state, not history: a restored peer starts offline/gray until the
            // connection or gossipsub announces it again.
            self.add_or_update_peer(peer_id, &peer.username, false);
        }
        for group in history.groups {
            let members = group
                .members
                .into_iter()
                .filter_map(|peer| peer.parse::<PeerId>().ok())
                .collect::<HashSet<_>>();
            self.groups.push(crate::ui::app::Group {
                id: group.id.clone(),
                name: group.name,
                members,
            });
            self.ensure_conversation(&group.id);
        }
        for message in history.messages {
            if !self.seen_messages.insert(message.id.clone()) {
                continue;
            }
            let mine = message.direction == "out";
            let peer = message.peer_id.parse::<PeerId>().ok();
            let key = if mine {
                message.channel.clone()
            } else if message.channel.starts_with("dm:") {
                match peer {
                    Some(peer) => format!("dm:{peer}"),
                    None => continue,
                }
            } else {
                match self.conversation_key_for(&message.channel) {
                    Some(key) => key,
                    None => continue,
                }
            };
            self.push_line(
                &key,
                ChatLine::Message {
                    id: message.id,
                    author: message.username_snapshot,
                    peer: if mine { Some(self.me) } else { peer },
                    body: message.body,
                    created_at: message.created_at,
                    mine,
                },
            );
        }
        for id in history.pending_message_ids {
            self.pending.entry(id).or_default();
        }
    }

    pub fn set_message_pending(&mut self, message_id: &str, pending: bool) {
        if pending {
            self.pending.entry(message_id.to_string()).or_default();
        } else {
            self.pending.remove(message_id);
        }
    }

    pub fn ensure_conversation(&mut self, key: &str) {
        if !self.conversations.contains_key(key) {
            self.conversations.insert(key.to_string(), Vec::new());
        }
    }

    pub fn add_or_update_peer(&mut self, peer_id: PeerId, username: &str, online: bool) {
        match self.peers.get_mut(&peer_id) {
            Some(peer) => {
                peer.username = username.to_string();
                peer.online = online;
            }
            None => {
                self.peers.insert(
                    peer_id,
                    Peer {
                        username: username.to_string(),
                        online,
                        color: color_for(&peer_id),
                    },
                );
            }
        }
        // A connection/presence event is enough to expose the DM channel; the user should not
        // have to send the first message before the peer appears in CHANNELS.
        self.ensure_conversation(&format!("dm:{peer_id}"));
    }

    pub fn set_peer_online(&mut self, peer_id: PeerId, online: bool) {
        if let Some(peer) = self.peers.get_mut(&peer_id) {
            peer.online = online;
        }
    }

    pub fn remove_peer(&mut self, peer_id: &PeerId) {
        self.peers.remove(peer_id);
    }

    pub fn peer_label(&self, peer_id: &PeerId) -> String {
        match self.peers.get(peer_id) {
            Some(peer) => display_name(&peer.username, peer_id),
            None => display_name("unknown", peer_id),
        }
    }

    pub fn sorted_peers(&self) -> Vec<PeerId> {
        let mut peers: Vec<PeerId> = self.peers.keys().copied().collect();
        peers.sort_by_key(|peer| (self.peer_label(peer), peer.to_string()));
        peers
    }

    pub fn conversation_keys(&self) -> Vec<String> {
        self.conversations.keys().cloned().collect()
    }

    /// Keys shown in the CHANNELS panel: `#general` plus DMs with a valid peer id.
    pub fn channel_keys(&self) -> Vec<String> {
        self.conversations
            .keys()
            .filter(|key| {
                key.as_str() == GENERAL
                    || key
                        .strip_prefix("dm:")
                        .is_some_and(|value| value.parse::<PeerId>().is_ok())
            })
            .cloned()
            .collect()
    }

    pub fn activate_conversation(&mut self, key: &str) {
        self.ensure_conversation(key);
        self.active_conversation = key.to_string();
        self.group_selection = self
            .groups
            .iter()
            .position(|group| group.id == key)
            .unwrap_or(self.group_selection);
    }

    pub fn system(&mut self, text: impl Into<String>) {
        self.ensure_conversation(&self.active_conversation.clone());
        let key = self.active_conversation.clone();
        self.push_line(&key, ChatLine::System(text.into()));
    }

    fn push_line(&mut self, key: &str, line: ChatLine) {
        let lines = self.conversations.entry(key.to_string()).or_default();
        lines.push(line);
        if lines.len() > MAX_LINES_PER_CONVERSATION {
            let excess = lines.len() - MAX_LINES_PER_CONVERSATION;
            lines.drain(..excess);
        }
    }

    /// Returns false for a duplicate, an unparseable DM sender, or a group we are not in.
    pub fn record_incoming(&mut self, message: ChatMessage) -> bool {
        if !self.seen_messages.insert(message.id.clone()) {
            return false;
        }
        let peer = message.from_peer.parse::<PeerId>().ok();
        // Any incoming message proves the sender is connected, including a `#general` broadcast.
        // Learn their name and online state so the UI shows them in green, not offline gray.
        if let Some(peer) = peer {
            self.add_or_update_peer(peer, &message.from_username, true);
        }
        let key = if message.channel.starts_with("dm:") {
            // The sender is the DM partner on our side, regardless of the channel string the
            // peer used. This keeps every DM in `dm:<peer_id>` instead of creating stray threads.
            match peer {
                Some(peer) => format!("dm:{peer}"),
                None => return false,
            }
        } else {
            match self.conversation_key_for(&message.channel) {
                Some(key) => key,
                None => return false,
            }
        };
        self.push_line(
            &key,
            ChatLine::Message {
                id: message.id,
                author: message.from_username,
                peer,
                body: message.body,
                created_at: message.created_at,
                mine: false,
            },
        );
        true
    }

    pub fn record_outgoing(&mut self, message: ChatMessage, recipients: Vec<PeerId>) {
        self.seen_messages.insert(message.id.clone());
        let key = message.channel.clone();
        self.ensure_conversation(&key);
        self.push_line(
            &key,
            ChatLine::Message {
                id: message.id.clone(),
                author: self.username.clone(),
                peer: Some(self.me),
                body: message.body,
                created_at: message.created_at,
                mine: true,
            },
        );
        self.mark_pending(&message.id, recipients);
    }

    pub fn mark_pending(&mut self, message_id: &str, recipients: Vec<PeerId>) {
        if recipients.is_empty() {
            return;
        }
        self.pending
            .insert(message_id.to_string(), recipients.into_iter().collect());
    }

    pub fn ack(&mut self, message_id: &str, peer_id: PeerId) {
        if let Some(waiting) = self.pending.get_mut(message_id) {
            waiting.remove(&peer_id);
            if waiting.is_empty() {
                self.pending.remove(message_id);
            }
        }
    }

    pub fn is_pending(&self, message_id: &str) -> bool {
        self.pending.contains_key(message_id)
    }

    /// `#general` is always visible; DMs are canonicalized by `record_incoming`, and group
    /// channels are only visible if we are a member.
    fn conversation_key_for(&self, channel: &str) -> Option<String> {
        if channel == GENERAL {
            return Some(channel.to_string());
        }
        self.groups
            .iter()
            .any(|group| group.id == channel)
            .then(|| channel.to_string())
    }

    /// Create a private group locally; no peer is contacted yet.
    pub fn create_group(&mut self, name: &str) -> String {
        let id = Uuid::new_v4().to_string();
        self.groups.push(Group {
            id: id.clone(),
            name: name.to_string(),
            members: HashSet::from([self.me]),
        });
        self.ensure_conversation(&id);
        id
    }

    /// True when we are a member of the group.
    pub fn is_group_member(&self, group_id: &str) -> bool {
        self.groups
            .iter()
            .any(|group| group.id == group_id && group.members.contains(&self.me))
    }

    pub fn group(&self, group_id: &str) -> Option<&Group> {
        self.groups.iter().find(|group| group.id == group_id)
    }

    /// Add a member to a group we belong to. Returns false for unknown groups or non-members.
    pub fn add_group_member(&mut self, group_id: &str, peer_id: PeerId) -> bool {
        match self.groups.iter_mut().find(|group| group.id == group_id) {
            Some(group) if group.members.contains(&self.me) => group.members.insert(peer_id),
            _ => false,
        }
    }

    /// A fresh invite is trusted; later `MemberAdded` events must come from an existing member.
    pub fn handle_group_invite(&mut self, from: PeerId, group_id: &str, group_name: &str) {
        if from == self.me || group_id.is_empty() {
            return;
        }
        match self.groups.iter_mut().find(|group| group.id == group_id) {
            Some(group) => {
                group.members.insert(from);
            }
            None => {
                self.groups.push(Group {
                    id: group_id.to_string(),
                    name: group_name.to_string(),
                    members: HashSet::from([self.me, from]),
                });
                self.ensure_conversation(group_id);
            }
        }
        self.activate_conversation(group_id);
        let who = self.peer_label(&from);
        self.system(format!(
            "➜ {who} added you to private group '{group_name}'"
        ));
    }

    pub fn handle_member_added(
        &mut self,
        from: PeerId,
        group_id: &str,
        added: PeerId,
        username: &str,
    ) -> bool {
        let group = match self.groups.iter_mut().find(|group| group.id == group_id) {
            Some(group) if group.members.contains(&self.me) && group.members.contains(&from) => group,
            _ => return false,
        };
        group.members.insert(added);
        let label = display_name(username, &added);
        let name = group.name.clone();
        self.system(format!("➜ {label} joined private group '{name}'"));
        true
    }

    pub fn resolve_user(&self, query: &str) -> Result<PeerId, ResolveError> {
        let query = query.trim();
        if let Ok(peer) = query.parse::<PeerId>() {
            if self.peers.contains_key(&peer) || peer == self.me {
                return Ok(peer);
            }
        }
        let exact: Vec<PeerId> = self
            .peers
            .iter()
            .filter(|(_, peer)| peer.username == query)
            .map(|(peer_id, _)| *peer_id)
            .collect();
        if exact.len() == 1 {
            return Ok(exact[0]);
        }
        let matches: Vec<(PeerId, String)> = self
            .peers
            .iter()
            .filter(|(peer_id, peer)| {
                peer.username.starts_with(query)
                    || self.peer_label(peer_id).starts_with(query)
            })
            .map(|(peer_id, peer)| (*peer_id, peer.username.clone()))
            .collect();
        match matches.len() {
            0 => Err(ResolveError::NotFound(query.to_string())),
            1 => Ok(matches[0].0),
            _ => Err(ResolveError::Ambiguous(
                query.to_string(),
                matches
                    .into_iter()
                    .map(|(peer, name)| display_name(&name, &peer))
                    .collect(),
            )),
        }
    }

    pub fn resolve_group(&self, query: &str) -> Result<usize, ResolveError> {
        let query = query.trim();
        let matches: Vec<usize> = self
            .groups
            .iter()
            .enumerate()
            .filter(|(_, group)| group.id == query || group.name == query || group.name.starts_with(query))
            .map(|(index, _)| index)
            .collect();
        match matches.len() {
            0 => Err(ResolveError::NotFound(query.to_string())),
            1 => Ok(matches[0]),
            _ => Err(ResolveError::Ambiguous(
                query.to_string(),
                matches
                    .iter()
                    .map(|index| self.groups[*index].name.clone())
                    .collect(),
            )),
        }
    }

    /// Enter list navigation from the input; choose the panel matching the active conversation.
    pub fn begin_selection(&mut self) {
        if self.selection != Selection::None {
            return;
        }
        if self.group(&self.active_conversation).is_some() {
            self.selection = Selection::Groups;
            self.group_selection = self
                .groups
                .iter()
                .position(|group| group.id == self.active_conversation)
                .unwrap_or(0);
        } else {
            self.selection = Selection::Channels;
            self.channel_selection = self
                .channel_keys()
                .iter()
                .position(|key| key == &self.active_conversation)
                .unwrap_or(0);
        }
    }

    pub fn move_selection(&mut self, delta: isize) {
        if self.selection == Selection::None {
            return;
        }
        let count = match self.selection {
            Selection::Channels => self.channel_keys().len(),
            Selection::Groups => self.groups.len(),
            Selection::None => 0,
        };
        if count == 0 {
            return;
        }
        let selected = if self.selection == Selection::Channels {
            &mut self.channel_selection
        } else {
            &mut self.group_selection
        };
        *selected = (*selected as isize + delta).rem_euclid(count as isize) as usize;
    }

    /// Enter/Tab on a highlighted channel switches to it.
    pub fn activate_selected_channel(&mut self) -> bool {
        let Some(key) = self.channel_keys().get(self.channel_selection).cloned() else {
            return false;
        };
        self.activate_conversation(&key);
        true
    }

    /// `/users` prints the known peers into the chat panel instead of using a side panel.
    pub fn users_summary(&self) -> String {
        if self.peers.is_empty() {
            return "No peers seen yet — wait for mDNS discovery".to_string();
        }
        let peers = self
            .sorted_peers()
            .into_iter()
            .map(|peer| {
                let marker = if self.peers[&peer].online {
                    "●"
                } else {
                    "○"
                };
                format!("{marker} {}", self.peer_label(&peer))
            })
            .collect::<Vec<_>>();
        format!("Users ({}): {}", peers.len(), peers.join(", "))
    }

    pub fn textarea_text(&self) -> String {
        self.textarea.lines().join("\n")
    }

    pub fn clear_textarea(&mut self) {
        self.textarea = tui_textarea::TextArea::default();
        self.textarea
            .set_placeholder_text("Type a message; Shift+Enter inserts a newline");
    }

    pub fn activate_selected_group(&mut self) -> bool {
        let Some(group) = self.groups.get(self.group_selection).cloned() else {
            return false;
        };
        self.activate_conversation(&group.id);
        true
    }

    pub fn cycle_conversation(&mut self) {
        let keys = self.conversation_keys();
        if keys.is_empty() {
            return;
        }
        let current = keys
            .iter()
            .position(|key| key == &self.active_conversation)
            .unwrap_or(0);
        self.activate_conversation(&keys[(current + 1) % keys.len()]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer_a() -> PeerId {
        "12D3KooWJ2q5Aj9fMZ43AFFz152XxBRQEDBsUo7hHKi4YpYJ4yyB"
            .parse()
            .unwrap()
    }

    fn peer_b() -> PeerId {
        "12D3KooWNgyWGoSc276cjs6MLNmtsGmNnspB8TdCQ22G69vXaDr4"
            .parse()
            .unwrap()
    }

    fn message(channel: &str, id: &str, body: &str) -> ChatMessage {
        ChatMessage {
            id: id.to_string(),
            from_peer: peer_b().to_string(),
            from_username: "bob".to_string(),
            channel: channel.to_string(),
            body: body.to_string(),
            created_at: 1,
            outbox_id: None,
        }
    }

    #[test]
    fn color_is_stable_and_distinct() {
        assert_eq!(color_for(&peer_a()), color_for(&peer_a()));
        assert_ne!(color_for(&peer_a()), color_for(&peer_b()));
        assert_eq!(OWN_COLOR, Color::Green);
    }

    #[test]
    fn incoming_messages_are_deduped_and_routed() {
        let mut app = App::new(peer_a(), "alice".to_string());
        assert!(app.record_incoming(message(GENERAL, "m1", "hello")));
        assert!(!app.record_incoming(message(GENERAL, "m1", "hello")));
        assert_eq!(app.conversations[GENERAL].len(), 1);
        // A broadcast also teaches us who is online, so it is shown in green.
        assert_eq!(app.peers[&peer_b()].username, "bob");
        assert!(app.peers[&peer_b()].online);

        // An incoming DM always lands in the canonical `dm:<sender>` thread, even if the
        // sender used a different channel string, and teaches us their username.
        let wrong_channel = format!("dm:{}", peer_a());
        assert!(app.record_incoming(message(&wrong_channel, "m2", "private")));
        let dm_key = format!("dm:{}", peer_b());
        assert_eq!(
            app.conversation_keys(),
            vec![GENERAL.to_string(), dm_key.clone()]
        );
        assert_eq!(app.peers[&peer_b()].username, "bob");
        assert!(app.peers[&peer_b()].online);

        // A DM with an unparseable sender must not create a stray channel.
        let bad_sender = ChatMessage {
            from_peer: "not-a-peer".to_string(),
            ..message("dm:not-a-peer", "m5", "ignored")
        };
        assert!(!app.record_incoming(bad_sender));
        assert_eq!(app.conversation_keys(), vec![GENERAL.to_string(), dm_key]);

        let group_id = "secret-group";
        assert!(!app.record_incoming(message(group_id, "m3", "hidden")));
        app.groups.push(Group {
            id: group_id.to_string(),
            name: "team".to_string(),
            members: HashSet::from([peer_a()]),
        });
        assert!(app.record_incoming(message(group_id, "m4", "visible")));
        assert_eq!(app.conversations[group_id].len(), 1);
    }

    #[test]
    fn pending_marker_clears_after_all_acks() {
        let mut app = App::new(peer_a(), "alice".to_string());
        app.mark_pending("m1", vec![peer_b()]);
        assert!(app.is_pending("m1"));
        app.ack("m1", peer_b());
        assert!(!app.is_pending("m1"));
        app.ack("m1", peer_b()); // idempotent
        assert!(!app.is_pending("m1"));

        app.mark_pending("m2", vec![peer_a(), peer_b()]);
        app.ack("m2", peer_a());
        assert!(app.is_pending("m2"));
        app.ack("m2", peer_b());
        assert!(!app.is_pending("m2"));
    }

    #[test]
    fn group_invite_and_chain_member_added_converge() {
        let mut alice = App::new(peer_a(), "alice".to_string());
        alice.handle_group_invite(peer_b(), "g1", "team");
        assert!(alice.is_group_member("g1"));
        assert!(alice.group("g1").unwrap().members.contains(&peer_b()));

        let bob_adds_carol = alice.group("g1").unwrap().members.contains(&peer_b());
        assert!(bob_adds_carol);

        let carol = PeerId::random();
        assert!(alice.handle_member_added(peer_b(), "g1", carol, "carol"));
        assert!(alice.group("g1").unwrap().members.contains(&carol));

        // A member_added from someone outside the group must be ignored.
        let stranger = PeerId::random();
        assert!(!alice.handle_member_added(stranger, "g1", PeerId::random(), "eve"));
        assert!(!alice.is_group_member("unknown"));
        assert!(!alice.handle_member_added(peer_b(), "unknown", carol, "carol"));
    }

    #[test]
    fn resolve_users_and_groups() {
        let mut app = App::new(peer_a(), "alice".to_string());
        app.add_or_update_peer(peer_b(), "bob", true);
        assert_eq!(app.resolve_user("bob").unwrap(), peer_b());
        assert_eq!(app.resolve_user("bo").unwrap(), peer_b());
        assert!(matches!(
            app.resolve_user("nobody"),
            Err(ResolveError::NotFound(_))
        ));

        app.add_or_update_peer(PeerId::random(), "bobby", true);
        assert!(matches!(
            app.resolve_user("bo"),
            Err(ResolveError::Ambiguous(_, _))
        ));

        let id = app.create_group("team");
        assert_eq!(app.resolve_group("team").unwrap(), 0);
        assert_eq!(app.resolve_group(&id).unwrap(), 0);
        assert!(app.resolve_group("missing").is_err());
    }

    #[test]
    fn channel_selection_and_users_summary() {
        let mut app = App::new(peer_a(), "alice".to_string());
        app.add_or_update_peer(peer_b(), "bob", true);
        assert!(app.users_summary().contains("bob·"));

        // The DM channel exists as soon as presence arrives, before any message is sent.
        let dm = format!("dm:{}", peer_b());
        assert_eq!(app.channel_keys(), vec![GENERAL.to_string(), dm.clone()]);
        app.activate_conversation(&dm);

        app.selection = Selection::Channels;
        app.move_selection(1);
        assert_eq!(app.channel_selection, 1);
        assert!(app.activate_selected_channel());
        assert_eq!(app.active_conversation, dm);

        app.move_selection(1);
        assert_eq!(app.channel_selection, 0); // wraps around
    }

    #[test]
    fn history_restores_peers_groups_messages_and_pending_state() {
        use crate::database::{History, StoredGroup, StoredMessage, StoredPeer};

        let mut app = App::new(peer_a(), "alice".to_string());
        let carol = PeerId::random();
        let history = History {
            peers: vec![
                StoredPeer {
                    peer_id: peer_b().to_string(),
                    username: "bob".to_string(),
                    is_local: false,
                    online: false,
                    first_seen: 1,
                    last_seen: 2,
                },
                StoredPeer {
                    peer_id: carol.to_string(),
                    username: "carol".to_string(),
                    is_local: true,
                    online: true,
                    first_seen: 1,
                    last_seen: 2,
                },
            ],
            messages: vec![
                StoredMessage {
                    id: "in-1".to_string(),
                    channel: GENERAL.to_string(),
                    peer_id: peer_b().to_string(),
                    username_snapshot: "bob".to_string(),
                    direction: "in".to_string(),
                    body: "old hello".to_string(),
                    created_at: 1,
                },
                StoredMessage {
                    id: "out-1".to_string(),
                    channel: format!("dm:{}", peer_b()),
                    peer_id: peer_b().to_string(),
                    username_snapshot: "alice".to_string(),
                    direction: "out".to_string(),
                    body: "queued".to_string(),
                    created_at: 2,
                },
            ],
            groups: vec![StoredGroup {
                id: "g1".to_string(),
                name: "team".to_string(),
                members: vec![peer_a().to_string(), peer_b().to_string()],
            }],
            pending_message_ids: HashSet::from(["out-1".to_string()]),
        };
        app.apply_history(history);

        assert!(app.peers.contains_key(&peer_b()));
        assert!(!app.peers.contains_key(&carol)); // local rows are not peers in the UI
        assert!(!app.peers[&peer_b()].online); // history does not imply live presence
        assert_eq!(app.conversations[GENERAL].len(), 1);
        assert_eq!(app.conversations[&format!("dm:{}", peer_b())].len(), 1);
        assert_eq!(app.groups[0].name, "team");
        assert!(app.is_group_member("g1"));
        assert!(app.is_pending("out-1"));

        app.set_message_pending("out-1", false);
        assert!(!app.is_pending("out-1"));
    }

    #[test]
    fn peer_color_is_set_once() {
        let mut app = App::new(peer_a(), "alice".to_string());
        app.add_or_update_peer(peer_b(), "bob", true);
        let color = app.peers[&peer_b()].color;
        app.add_or_update_peer(peer_b(), "bobby", false);
        assert_eq!(app.peers[&peer_b()].color, color);
        assert_eq!(app.peers[&peer_b()].username, "bobby");
        assert!(!app.peers[&peer_b()].online);
    }
}
