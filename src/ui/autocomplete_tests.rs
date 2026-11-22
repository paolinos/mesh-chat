//! End-to-end tests for the autocomplete use case:
//! "I don't want to type the whole command, and I want the username completed too."

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use libp2p::PeerId;
use ratatui::{backend::TestBackend, buffer::Buffer, Terminal};

use super::{
    app::{App, GENERAL},
    input::{handle_key, Action, InputResult},
    render,
};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent {
        code,
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    }
}

fn press(app: &mut App<'_>, code: KeyCode) -> InputResult {
    handle_key(app, key(code))
}

fn type_text(app: &mut App<'_>, text: &str) {
    for ch in text.chars() {
        press(app, KeyCode::Char(ch));
    }
}

fn screen(app: &App<'_>) -> String {
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

#[cfg(test)]
mod scenario_tests {
    use super::*;

    #[test]
    fn typing_slash_u_then_tab_completes_command_and_username() {
        let bob = PeerId::random();
        let bobby = PeerId::random();
        let mut app = App::new(PeerId::random(), "me".to_string());
        app.add_or_update_peer(bob, "bob", true);
        app.add_or_update_peer(bobby, "bobby", false);

        // User only types the first letter of the command.
        type_text(&mut app, "/u");
        assert_eq!(app.completion.as_ref().unwrap().candidates[0].value, "/users");
        let hint = screen(&app);
        assert!(hint.contains("/users"), "hint should show the full command: {hint}");
        assert!(hint.contains("Tab"), "hint should explain the accept key: {hint}");

        // Tab inserts the whole command and immediately offers the known usernames.
        assert_eq!(press(&mut app, KeyCode::Tab), InputResult::Redraw);
        assert_eq!(app.textarea_text(), "/users ");
        assert_eq!(app.completion.as_ref().unwrap().candidates[0].value, "bob");
        let hint = screen(&app);
        assert!(hint.contains("bob"), "hint should list usernames: {hint}");

        // Narrow to "bob" and accept it.
        press(&mut app, KeyCode::Char('b'));
        let values: Vec<_> = app
            .completion
            .as_ref()
            .unwrap()
            .candidates
            .iter()
            .map(|candidate| candidate.value.as_str())
            .collect();
        assert_eq!(values, vec!["bob", "bobby"]);
        assert!(screen(&app).contains("also: bobby"));

        press(&mut app, KeyCode::Tab);
        assert_eq!(app.textarea_text(), "/users bob ");
        assert!(app.completion.is_none());

        // The body can be typed normally; submitting produces a DM to the completed username.
        type_text(&mut app, "hello");
        assert_eq!(
            press(&mut app, KeyCode::Enter),
            InputResult::Action(Action::SendDm {
                user: "bob".to_string(),
                body: "hello".to_string(),
            })
        );
        assert_eq!(app.textarea_text(), "");
        assert_eq!(app.resolve_user("bob").unwrap(), bob);
    }

    #[test]
    fn unambiguous_prefix_works_without_pressing_tab() {
        let mut app = App::new(PeerId::random(), "me".to_string());
        type_text(&mut app, "/u");
        assert_eq!(
            press(&mut app, KeyCode::Enter),
            InputResult::Action(Action::Users)
        );

        type_text(&mut app, "/g cr team");
        assert_eq!(
            press(&mut app, KeyCode::Enter),
            InputResult::Action(Action::GroupCreate("team".to_string()))
        );
    }

    #[test]
    fn tab_keeps_its_old_job_when_there_is_no_hint() {
        let mut app = App::new(PeerId::random(), "me".to_string());
        let bob = PeerId::random();
        app.add_or_update_peer(bob, "bob", true);
        app.activate_conversation(&format!("dm:{bob}"));
        assert!(app.completion.is_none());
        assert_eq!(press(&mut app, KeyCode::Tab), InputResult::Redraw);
        assert_eq!(app.active_conversation, GENERAL);
    }
}
