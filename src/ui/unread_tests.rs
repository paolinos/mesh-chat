//! End-to-end tests for unread bullets on conversations (DMs, groups and #general).

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use libp2p::PeerId;
use ratatui::{backend::TestBackend, buffer::Buffer, Terminal};

use super::{
    app::{make_message, App},
    handle_network_event,
    input::handle_key,
    render,
};
use crate::network::NetworkEvent;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent {
        code,
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    }
}

fn render_screen(app: &App<'_>) -> String {
    let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
    terminal.draw(|frame| render::render(frame, app)).unwrap();
    let buffer: &Buffer = terminal.backend().buffer();
    let mut text = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            text.push_str(buffer[(x, y)].symbol());
        }
        text.push('\n');
    }
    text
}

fn receive(app: &mut App<'_>, from: PeerId, username: &str, channel: &str, body: &str) {
    handle_network_event(
        app,
        NetworkEvent::MessageReceived {
            message: make_message(&from, username, channel.to_string(), body.to_string()),
        },
        None,
    );
}

#[cfg(test)]
mod scenario_tests {
    use super::*;

    #[test]
    fn unread_bullet_appears_until_the_conversation_is_opened() {
        let bob = PeerId::random();
        let mut app = App::new(PeerId::random(), "alice".to_string());
        app.add_or_update_peer(bob, "bob", true);
        let dm = format!("dm:{bob}");
        let group_id = app.create_group("team");

        // A message in the conversation already open is read straight away.
        receive(&mut app, bob, "bob", super::super::app::GENERAL, "in general");
        assert!(!app.has_unread(super::super::app::GENERAL));

        // A DM arriving while we are in #general is marked unread next to the username.
        receive(&mut app, bob, "bob", &dm, "private hello");
        assert!(app.has_unread(&dm));
        assert!(render_screen(&app)
            .lines()
            .any(|line| line.contains("DM bob") && line.contains('●')));

        // Navigate to the DM with the keyboard; opening it clears the bullet.
        handle_key(&mut app, key(KeyCode::Up));
        handle_key(&mut app, key(KeyCode::Enter));
        assert_eq!(app.active_conversation, dm);
        assert!(!app.has_unread(&dm));
        let screen = render_screen(&app);
        let dm_row = screen
            .lines()
            .find(|line| line.starts_with("│DM bob"))
            .unwrap();
        assert!(!dm_row.contains('●'));

        // A group message arriving while we are in the DM gets its own bullet.
        receive(&mut app, bob, "bob", &group_id, "group hello");
        assert!(app.has_unread(&group_id));
        assert!(render_screen(&app)
            .lines()
            .any(|line| line.contains("team") && line.contains('●')));

        // Opening the group clears it too.
        app.activate_conversation(&group_id);
        assert!(!app.has_unread(&group_id));
        let screen = render_screen(&app);
        let group_row = screen
            .lines()
            .find(|line| line.starts_with("│🔒"))
            .unwrap();
        assert!(!group_row.contains('●'));
    }

    #[test]
    fn unread_marker_survives_restart_and_disappears_after_read() {
        use crate::database::Database;

        let dir = tempfile::tempdir().unwrap();
        let mut database = Database::open(&dir.path().join("chat.db")).unwrap();
        let bob = PeerId::random();
        let dm = format!("dm:{bob}");
        let message = make_message(&bob, "bob", dm.clone(), "unread on restart".to_string());

        // Simulate receiving a message while the DM is not open: message and marker are written
        // together.
        assert!(database
            .insert_incoming_message(&message, &dm, true, &[])
            .unwrap());
        let history = database.history().unwrap();
        assert_eq!(history.unread.len(), 1);

        // A fresh app after a restart still shows the bullet.
        let mut app = App::new(PeerId::random(), "alice".to_string());
        app.apply_history(history);
        assert!(app.has_unread(&dm));
        assert!(render_screen(&app)
            .lines()
            .any(|line| line.contains('●')));

        // Opening the conversation deletes the marker, like `Store::mark_conversation_read` does.
        app.activate_conversation(&dm);
        database.clear_unread(&dm).unwrap();
        assert!(database.history().unwrap().unread.is_empty());

        // A second restart shows no bullet.
        let mut app = App::new(PeerId::random(), "alice".to_string());
        app.apply_history(database.history().unwrap());
        assert!(!app.has_unread(&dm));
    }

    #[tokio::test]
    async fn received_dm_is_stored_with_canonical_key_and_cleared_on_read() {
        use crate::worker::Store;

        let dir = tempfile::tempdir().unwrap();
        let (network_tx, _network_rx) = tokio::sync::mpsc::channel(4);
        let (store, handle) = Store::spawn(
            &dir.path().join("chat.db"),
            dir.path().to_path_buf(),
            PeerId::random(),
            "alice".to_string(),
            network_tx,
        )
        .unwrap();

        let bob = PeerId::random();
        let mut app = App::new(PeerId::random(), "alice".to_string());
        let dm = format!("dm:{bob}");

        // The sender may use our id in the channel; the UI normalizes it to `dm:<sender>` and
        // stores that same key in SQLite.
        let sender_channel = format!("dm:{}", app.me);
        handle_network_event(
            &mut app,
            NetworkEvent::MessageReceived {
                message: make_message(&bob, "bob", sender_channel, "hello".to_string()),
            },
            Some(&store),
        );
        assert!(app.has_unread(&dm));

        let history = store.load_history().await.unwrap();
        assert_eq!(history.unread.len(), 1);
        assert_eq!(history.unread[0].channel, dm);

        // Opening the conversation removes the stored row.
        app.activate_conversation(&dm);
        store.mark_conversation_read(&dm);
        let history = store.load_history().await.unwrap();
        assert!(history.unread.is_empty());

        store.shutdown();
        let _ = handle.task.await;
    }
}
