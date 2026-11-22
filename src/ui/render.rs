use time::{OffsetDateTime, UtcOffset};

use ratatui::{
    layout::{Alignment, Constraint, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, List, ListItem, Paragraph},
    Frame,
};

use super::app::{App, AttachmentHitArea, ChatLine, Selection, GENERAL, OWN_COLOR};

/// Render a stored epoch-millisecond timestamp as local `HH:MM`.
/// Falls back to UTC if the platform cannot determine the local offset.
fn format_time(created_at: i64) -> String {
    let stamp = OffsetDateTime::from_unix_timestamp(created_at.div_euclid(1_000))
        .unwrap_or(OffsetDateTime::UNIX_EPOCH);
    let local = stamp.to_offset(UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC));
    format!("{:02}:{:02}", local.hour(), local.minute())
}

fn selected(active: bool) -> Style {
    if active {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default()
    }
}

/// Online peers are green; offline peers are gray. Selection adds reverse video.
fn peer_style(app: &App, peer_id: &libp2p::PeerId, active: bool) -> Style {
    let online = app.peers.get(peer_id).is_some_and(|peer| peer.online);
    let mut style = Style::default().fg(if online {
        Color::Green
    } else {
        Color::DarkGray
    });
    if active {
        style = style.add_modifier(Modifier::REVERSED);
    }
    style
}

/// A yellow bullet on the right of a conversation label means unread messages.
fn unread_bullet(app: &App, key: &str) -> Span<'static> {
    if app.has_unread(key) {
        Span::styled(
            " ●",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::raw("")
    }
}

fn channels_list(app: &App) -> Vec<ListItem<'static>> {
    app.channel_keys()
        .into_iter()
        .enumerate()
        .filter_map(|(index, key)| {
            let active = (app.selection == Selection::Channels
                && index == app.channel_selection)
                || app.active_conversation == key;
            if key == GENERAL {
                Some(ListItem::new(Line::from(vec![
                    Span::styled(key.clone(), selected(active)),
                    unread_bullet(app, &key),
                ])))
            } else {
                let peer_id = key
                    .strip_prefix("dm:")?
                    .parse::<libp2p::PeerId>()
                    .ok()?;
                Some(ListItem::new(Line::from(vec![
                    Span::styled(
                        format!("DM {}", app.peer_label(&peer_id)),
                        peer_style(app, &peer_id, active),
                    ),
                    unread_bullet(app, &key),
                ])))
            }
        })
        .collect()
}

fn groups_list(app: &App) -> Vec<ListItem<'static>> {
    app.groups
        .iter()
        .enumerate()
        .map(|(index, group)| {
            let active = (app.selection == Selection::Groups && index == app.group_selection)
                || app.active_conversation == group.id;
            ListItem::new(Line::from(vec![
                Span::styled(format!("🔒 {}", group.name), selected(active)),
                unread_bullet(app, &group.id),
            ]))
        })
        .collect()
}

fn header(app: &App) -> Line<'static> {
    let mut spans = Vec::new();
    let keys = app.conversation_keys();
    for (index, key) in keys.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw(" | "));
        }
        let dm_peer = key
            .strip_prefix("dm:")
            .and_then(|value| value.parse::<libp2p::PeerId>().ok());
        let label = if let Some(group) = app.group(key) {
            format!("🔒 {}", group.name)
        } else if let Some(peer_id) = dm_peer {
            format!("DM {}", app.peer_label(&peer_id))
        } else {
            key.clone()
        };
        let style = if key == &app.active_conversation {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else if let Some(peer_id) = dm_peer {
            peer_style(app, &peer_id, false)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        spans.push(Span::styled(label, style));
        spans.push(unread_bullet(app, key));
    }
    Line::from(spans)
}

/// The completion hint sits on its own row inside the input panel.
fn completion_hint(app: &App) -> Option<Line<'static>> {
    let completion = app.completion.as_ref()?;
    let best = completion.candidates.first()?;
    let mut spans = vec![
        Span::styled(
            "Tab ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("→ "),
        Span::styled(best.label.clone(), Style::default().fg(Color::Cyan)),
    ];
    if completion.candidates.len() > 1 {
        let others = completion.candidates[1..]
            .iter()
            .map(|candidate| candidate.label.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        spans.push(Span::styled(
            format!("  (also: {others})"),
            Style::default().fg(Color::DarkGray),
        ));
    }
    spans.push(Span::styled(
        "  Esc to dismiss".to_string(),
        Style::default().fg(Color::DarkGray),
    ));
    Some(Line::from(spans))
}

/// Hint showing files queued by `/file` or `/attach`.
fn attachment_hint(app: &App) -> Option<Line<'static>> {
    if app.pending_attachments.is_empty() {
        return None;
    }
    let names: Vec<String> = app
        .pending_attachments
        .iter()
        .filter_map(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.to_string())
        })
        .collect();
    Some(Line::from(vec![
        Span::styled("📎 ", Style::default().fg(Color::Cyan)),
        Span::styled(
            names.join(", "),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" attached — press Enter to send", Style::default().fg(Color::DarkGray)),
    ]))
}

fn chat_lines(app: &App) -> (Vec<Line<'static>>, Vec<(usize, AttachmentHitArea)>) {
    let mut rendered = Vec::new();
    let mut attachment_areas = Vec::new();
    let Some(lines) = app.conversations.get(&app.active_conversation) else {
        return (rendered, attachment_areas);
    };

    for line in lines {
        match line {
            ChatLine::System(text) => rendered.push(Line::from(Span::styled(
                text.clone(),
                Style::default()
                    .fg(Color::Gray)
                    .add_modifier(Modifier::ITALIC),
            ))),
            ChatLine::Message {
                id,
                author,
                peer,
                body,
                created_at,
                mine,
                attachments,
            } => {
                // A live (online) sender is green, matching their green channel label;
                // offline or unknown senders are gray.
                let color = if *mine {
                    OWN_COLOR
                } else if peer
                    .as_ref()
                    .is_some_and(|peer| app.peers.get(peer).is_some_and(|peer| peer.online))
                {
                    Color::Green
                } else {
                    Color::Gray
                };
                let name = if *mine { "me" } else { author.as_str() };
                let suffix = if app.is_pending(id) {
                    " (Pending)"
                } else {
                    ""
                };
                let first_prefix = format!("[{}] {name}: ", format_time(*created_at));
                let continuation = " ".repeat(first_prefix.chars().count());
                for (index, part) in body.split('\n').enumerate() {
                    let prefix = if index == 0 {
                        first_prefix.clone()
                    } else {
                        continuation.clone()
                    };
                    let mut spans = vec![
                        Span::styled(prefix, Style::default().fg(color)),
                        Span::styled(part.to_string(), Style::default().fg(color)),
                    ];
                    if !suffix.is_empty() && index == 0 {
                        spans.push(Span::styled(
                            suffix.to_string(),
                            Style::default().fg(Color::Yellow),
                        ));
                    }
                    let mut rendered_line = Line::from(spans);
                    if *mine {
                        rendered_line = rendered_line.alignment(Alignment::Right);
                    }
                    rendered.push(rendered_line);
                }
                for attachment in attachments {
                    let text = format!("📎 {} → {}", attachment.filename, attachment.display_path);
                    let width = text.chars().count() as u16;
                    attachment_areas.push((
                        rendered.len(),
                        AttachmentHitArea {
                            x: 0,
                            y: 0,
                            width,
                            height: 1,
                            open_path: attachment.open_path.clone(),
                        },
                    ));
                    rendered.push(Line::from(Span::styled(
                        text,
                        Style::default().fg(Color::Cyan),
                    )));
                }
            }
        }
    }
    (rendered, attachment_areas)
}

pub fn render(frame: &mut Frame, app: &App) {
    let [left, right] = Layout::horizontal([Constraint::Length(25), Constraint::Min(20)])
        .areas(frame.area());
    let [channels, groups] =
        Layout::vertical([Constraint::Percentage(60), Constraint::Percentage(40)]).areas(left);

    frame.render_widget(
        List::new(channels_list(app)).block(Block::bordered().title("CHANNELS")),
        channels,
    );
    frame.render_widget(
        List::new(groups_list(app)).block(Block::bordered().title("GROUPS")),
        groups,
    );

    let completion = completion_hint(app);
    let attachment = attachment_hint(app);
    let input_height: u16 = app
        .textarea
        .lines()
        .len()
        .clamp(3, 10)
        .try_into()
        .unwrap_or(3);
    let input_height = input_height + u16::from(completion.is_some()) + u16::from(attachment.is_some());
    let [head, chat, input] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(input_height),
    ])
    .areas(right);

    frame.render_widget(header(app), head);

    let chat_block = Block::bordered();
    let chat_inner = chat_block.inner(chat);
    frame.render_widget(chat_block, chat);
    app.attachment_hit_areas.borrow_mut().clear();
    let (lines, attachment_areas) = chat_lines(app);
    let start = lines.len().saturating_sub(chat_inner.height as usize);
    frame.render_widget(Paragraph::new(lines[start..].to_vec()), chat_inner);
    for (line_index, mut area) in attachment_areas {
        let visible_index = line_index.saturating_sub(start);
        if visible_index >= chat_inner.height as usize {
            continue;
        }
        area.x = chat_inner.x;
        area.y = chat_inner.y + visible_index as u16;
        app.attachment_hit_areas.borrow_mut().push(area);
    }

    let input_block = Block::bordered().title("Message (Tab = complete, Shift+Enter = newline)");
    let input_inner = input_block.inner(input);
    frame.render_widget(input_block, input);
    let hint_count = u16::from(completion.is_some()) + u16::from(attachment.is_some());
    if hint_count > 0 {
        let mut constraints = vec![Constraint::Min(1)];
        if completion.is_some() {
            constraints.push(Constraint::Length(1));
        }
        if attachment.is_some() {
            constraints.push(Constraint::Length(1));
        }
        let areas: Vec<_> = Layout::vertical(constraints).split(input_inner).to_vec();
        frame.render_widget(&app.textarea, areas[0]);
        let mut next = 1;
        if let Some(line) = completion {
            frame.render_widget(Paragraph::new(line), areas[next]);
            next += 1;
        }
        if let Some(line) = attachment {
            frame.render_widget(Paragraph::new(line), areas[next]);
        }
    } else {
        frame.render_widget(&app.textarea, input_inner);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::app::make_message;
    use libp2p::PeerId;
    use ratatui::{backend::TestBackend, buffer::Buffer, Terminal};

    fn buffer_text(buffer: &Buffer) -> String {
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        text
    }

    fn rendered(app: &App) -> String {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, app)).unwrap();
        buffer_text(terminal.backend().buffer())
    }

    fn row_with<'a>(screen: &'a str, needle: &str) -> &'a str {
        screen
            .lines()
            .find(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("no row containing {needle}"))
    }

    #[test]
    fn renders_panels_and_alignment() {
        let me: PeerId = "12D3KooWJ2q5Aj9fMZ43AFFz152XxBRQEDBsUo7hHKi4YpYJ4yyB"
            .parse()
            .unwrap();
        let bob = PeerId::random();
        let mut app = App::new(me, "alice".to_string());
        app.add_or_update_peer(bob, "bob", true);
        let group_id = app.create_group("team");
        app.activate_conversation(GENERAL);
        let incoming_message = make_message(&me, "bob", GENERAL.to_string(), "hey from bob".to_string());
        let outgoing_message =
            make_message(&me, "alice", GENERAL.to_string(), "hi from me".to_string());
        app.record_incoming(incoming_message.clone());
        app.record_outgoing(outgoing_message.clone(), vec![bob], Vec::new());

        let screen = rendered(&app);
        assert!(!screen.contains("USERS"));
        assert!(screen.contains("CHANNELS"));
        assert!(screen.contains("GROUPS"));
        assert!(screen.contains("#general"));
        assert!(screen.contains("bob"));
        assert!(screen.contains("team"));
        assert!(screen.contains("hey from bob"));
        assert!(screen.contains("hi from me"));
        assert!(screen.contains("(Pending)"));

        let incoming = row_with(&screen, "hey from bob");
        let outgoing = row_with(&screen, "hi from me");
        assert!(incoming.contains(&format_time(incoming_message.created_at)));
        assert!(outgoing.contains(&format_time(outgoing_message.created_at)));
        assert!(outgoing.contains("(Pending)"));
        let incoming_col = incoming.find("hey from bob").unwrap();
        let outgoing_col = outgoing.find("hi from me").unwrap();
        assert!(incoming_col < outgoing_col, "incoming text should be left of own text");
        assert!(incoming_col < 50, "incoming text should be left aligned");
        assert!(outgoing_col > 55, "own text should be right aligned");

        assert!(app.group(&group_id).is_some());
    }

    #[test]
    fn unread_bullets_appear_on_dm_and_group_labels() {
        let me: PeerId = "12D3KooWJ2q5Aj9fMZ43AFFz152XxBRQEDBsUo7hHKi4YpYJ4yyB"
            .parse()
            .unwrap();
        let bob = PeerId::random();
        let mut app = App::new(me, "alice".to_string());
        app.add_or_update_peer(bob, "bob", true);
        let dm = format!("dm:{bob}");
        let group_id = app.create_group("team");

        app.record_incoming(make_message(&bob, "bob", dm.clone(), "dm unread".to_string()));
        app.record_incoming(make_message(
            &bob,
            "bob",
            group_id.clone(),
            "group unread".to_string(),
        ));

        let screen = rendered(&app);
        assert!(screen
            .lines()
            .any(|line| line.contains("DM bob") && line.contains('●')));
        assert!(screen
            .lines()
            .any(|line| line.contains("team") && line.contains('●')));

        app.activate_conversation(&dm);
        let screen = rendered(&app);
        // Check the panel row itself: the header is one line shared by all conversations.
        let dm_row = screen
            .lines()
            .find(|line| line.starts_with("│DM bob"))
            .unwrap();
        assert!(!dm_row.contains('●'));
        assert!(screen
            .lines()
            .any(|line| line.contains("team") && line.contains('●')));
    }

    #[test]
    fn shows_completion_hint_below_the_input() {
        let mut app = App::new(PeerId::random(), "me".to_string());
        app.add_or_update_peer(PeerId::random(), "bob", true);
        app.textarea.insert_str("/users b");
        app.refresh_completion();

        let screen = rendered(&app);
        assert!(screen.contains("/users "));
        assert!(screen.contains("Tab"));
        assert!(screen.contains("bob"));
        assert!(screen.contains("Esc to dismiss"));
    }

    #[test]
    fn renders_attachments_with_filename_and_path() {
        let me: PeerId = "12D3KooWJ2q5Aj9fMZ43AFFz152XxBRQEDBsUo7hHKi4YpYJ4yyB"
            .parse()
            .unwrap();
        let bob = PeerId::random();
        let mut app = App::new(me, "alice".to_string());
        app.add_or_update_peer(bob, "bob", true);
        let dm = format!("dm:{bob}");
        app.activate_conversation(&dm);

        let mut message = make_message(&bob, "bob", dm.clone(), "see attached".to_string());
        message.attachments = vec![crate::network::protocol::Attachment {
            filename: "report.pdf".to_string(),
            bytes: Vec::new(),
            mime_type: None,
        }];
        app.record_incoming(message);

        app.add_pending_attachment(std::path::PathBuf::from("/tmp/draft.txt"));

        let screen = rendered(&app);
        assert!(screen.contains("report.pdf"), "attachment filename not found");
        assert!(screen.contains("(saving...)"), "placeholder path not found");
        assert!(screen.contains("draft.txt"), "pending attachment hint not found");

        // Hit area is recorded for the rendered attachment.
        assert!(!app.attachment_hit_areas.borrow().is_empty());
    }

    #[test]
    fn dm_channel_labels_use_online_colors() {
        let me: PeerId = "12D3KooWJ2q5Aj9fMZ43AFFz152XxBRQEDBsUo7hHKi4YpYJ4yyB"
            .parse()
            .unwrap();
        let bob = PeerId::random();
        let carol = PeerId::random();
        let mut app = App::new(me, "alice".to_string());
        app.add_or_update_peer(bob, "bob", true);
        app.add_or_update_peer(carol, "carol", false);
        app.activate_conversation(&format!("dm:{bob}"));
        app.activate_conversation(&format!("dm:{carol}"));

        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal.draw(|frame| render(frame, &app)).unwrap();
        let screen = buffer_text(terminal.backend().buffer());

        // Only inspect the left panel: the right-hand header uses yellow for the active tab.
        let fg_at = |needle: &str| {
            for (row, line) in screen.lines().enumerate() {
                let left: String = line.chars().take(25).collect();
                if let Some(byte_col) = left.find(needle) {
                    // Buffer coordinates are characters, but `find` returns a byte offset.
                    let col = left[..byte_col].chars().count();
                    return terminal.backend().buffer()[(col as u16, row as u16)].fg;
                }
            }
            panic!("no left-panel row containing {needle}");
        };

        assert_eq!(fg_at(&format!("DM {}", app.peer_label(&bob))), Color::Green);
        assert_eq!(
            fg_at(&format!("DM {}", app.peer_label(&carol))),
            Color::DarkGray
        );
    }

    #[test]
    fn broadcast_colors_follow_online_state() {
        let me: PeerId = "12D3KooWJ2q5Aj9fMZ43AFFz152XxBRQEDBsUo7hHKi4YpYJ4yyB"
            .parse()
            .unwrap();
        let bob = PeerId::random();
        let carol = PeerId::random();
        let mut app = App::new(me, "alice".to_string());
        app.record_incoming(make_message(
            &bob,
            "bob",
            GENERAL.to_string(),
            "online broadcast".to_string(),
        ));
        app.record_incoming(make_message(
            &carol,
            "carol",
            GENERAL.to_string(),
            "offline broadcast".to_string(),
        ));
        app.set_peer_online(carol, false);

        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal.draw(|frame| render(frame, &app)).unwrap();
        let screen = buffer_text(terminal.backend().buffer());

        let fg_at = |needle: &str| {
            for (row, line) in screen.lines().enumerate() {
                if let Some(byte_col) = line.find(needle) {
                    let col = line[..byte_col].chars().count();
                    return terminal.backend().buffer()[(col as u16, row as u16)].fg;
                }
            }
            panic!("no row containing {needle}");
        };

        assert_eq!(fg_at("online broadcast"), Color::Green);
        assert_eq!(fg_at("offline broadcast"), Color::Gray);
    }
}
