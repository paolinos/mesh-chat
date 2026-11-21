use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tui_textarea::Input;

use super::app::{App, Selection};

pub const HELP: &str =
    "Commands: /users [name] [message], /group create <name>, /group add <group> <user>, /group members <group>, /quit";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Ignore,
    Chat(String),
    Users,
    Dm { user: String, body: String },
    GroupCreate(String),
    GroupAdd { group: String, user: String },
    GroupMembers(String),
    Quit,
    Help,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    SendChat(String),
    SendDm { user: String, body: String },
    Users,
    GroupCreate(String),
    GroupAdd { group: String, user: String },
    GroupMembers(String),
    Help(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputResult {
    Redraw,
    Quit,
    Action(Action),
}

/// Pure command parser shared by the key handler and unit tests.
pub fn parse_input(line: &str) -> Command {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Command::Ignore;
    }
    if !trimmed.starts_with('/') {
        return Command::Chat(trimmed.to_string());
    }

    let (first, rest) = match trimmed.split_once('\n') {
        Some((head, tail)) => (head.trim(), tail),
        None => (trimmed, ""),
    };

    if first == "/quit" || first == "/exit" {
        return Command::Quit;
    }
    if first == "/users" {
        return Command::Users;
    }
    if let Some(args) = first.strip_prefix("/users ") {
        let mut parts = args.trim().splitn(2, char::is_whitespace);
        let user = parts.next().unwrap_or("").to_string();
        if user.is_empty() {
            return Command::Users;
        }
        let inline = parts.next().unwrap_or("").trim();
        let body = match (inline.is_empty(), rest.is_empty()) {
            (true, _) => rest.to_string(),
            (false, true) => inline.to_string(),
            (false, false) => format!("{inline}\n{rest}"),
        };
        return Command::Dm { user, body };
    }
    if let Some(args) = first.strip_prefix("/group ") {
        let args = args.trim();
        if let Some(name) = args.strip_prefix("create ") {
            let name = name.trim();
            return if name.is_empty() {
                Command::Help
            } else {
                Command::GroupCreate(name.to_string())
            };
        }
        if let Some(arg) = args.strip_prefix("members ") {
            return Command::GroupMembers(arg.trim().to_string());
        }
        if let Some(arg) = args.strip_prefix("add ") {
            // Split from the right so group names may contain spaces.
            return match arg.trim().rsplit_once(char::is_whitespace) {
                Some((group, user)) if !group.is_empty() && !user.is_empty() => Command::GroupAdd {
                    group: group.to_string(),
                    user: user.to_string(),
                },
                _ => Command::Help,
            };
        }
    }
    Command::Help
}

fn command_to_action(command: Command) -> InputResult {
    match command {
        Command::Ignore | Command::Chat(_) => InputResult::Redraw,
        Command::Quit => InputResult::Quit,
        Command::Users => InputResult::Action(Action::Users),
        Command::Dm { user, body } => InputResult::Action(Action::SendDm { user, body }),
        Command::GroupCreate(name) => InputResult::Action(Action::GroupCreate(name)),
        Command::GroupAdd { group, user } => InputResult::Action(Action::GroupAdd { group, user }),
        Command::GroupMembers(group) => InputResult::Action(Action::GroupMembers(group)),
        Command::Help => InputResult::Action(Action::Help(HELP.to_string())),
    }
}

/// Route a single crossterm key event. Editing keys go to the textarea; navigation and
/// submission are handled here so they can be unit tested without a terminal.
pub fn handle_key(app: &mut App, key: KeyEvent) -> InputResult {
    if key.kind == KeyEventKind::Release {
        return InputResult::Redraw;
    }

    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
    {
        return InputResult::Quit;
    }

    match key.code {
        KeyCode::Up | KeyCode::Down => {
            app.move_selection(if key.code == KeyCode::Up { -1 } else { 1 });
            InputResult::Redraw
        }
        KeyCode::Tab => {
            match app.selection {
                Selection::Channels => {
                    app.activate_selected_channel();
                }
                Selection::Groups => {
                    app.activate_selected_group();
                }
                Selection::None => app.cycle_conversation(),
            }
            InputResult::Redraw
        }
        KeyCode::Esc => {
            app.selection = Selection::None;
            InputResult::Redraw
        }
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
            app.textarea.input(Input {
                key: tui_textarea::Key::Enter,
                ctrl: false,
                alt: false,
                shift: true,
            });
            InputResult::Redraw
        }
        KeyCode::Enter => match app.selection {
            Selection::Channels => {
                app.activate_selected_channel();
                InputResult::Redraw
            }
            Selection::Groups => {
                app.activate_selected_group();
                InputResult::Redraw
            }
            Selection::None => {
                let text = app.textarea_text();
                app.clear_textarea();
                match parse_input(&text) {
                    Command::Chat(body) => InputResult::Action(Action::SendChat(body)),
                    command => command_to_action(command),
                }
            }
        },
        _ => {
            app.textarea.input(Input::from(key));
            InputResult::Redraw
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::app::GENERAL;
    use libp2p::PeerId;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        }
    }

    #[test]
    fn parses_plain_and_slash_commands() {
        assert_eq!(parse_input("  "), Command::Ignore);
        assert_eq!(parse_input("hello world"), Command::Chat("hello world".to_string()));
        assert_eq!(parse_input("/quit"), Command::Quit);
        assert_eq!(parse_input("/users"), Command::Users);
        assert_eq!(parse_input("/help"), Command::Help);
        assert_eq!(parse_input("/bogus"), Command::Help);
    }

    #[test]
    fn parses_dm_with_multiline_body() {
        assert_eq!(
            parse_input("/users bob hello"),
            Command::Dm {
                user: "bob".to_string(),
                body: "hello".to_string()
            }
        );
        assert_eq!(
            parse_input("/users bob\nline one\nline two"),
            Command::Dm {
                user: "bob".to_string(),
                body: "line one\nline two".to_string()
            }
        );
    }

    #[test]
    fn parses_group_commands() {
        assert_eq!(
            parse_input("/group create team"),
            Command::GroupCreate("team".to_string())
        );
        assert_eq!(
            parse_input("/group add my team bob"),
            Command::GroupAdd {
                group: "my team".to_string(),
                user: "bob".to_string()
            }
        );
        assert_eq!(
            parse_input("/group members team"),
            Command::GroupMembers("team".to_string())
        );
        assert_eq!(parse_input("/group add bob"), Command::Help);
        assert_eq!(parse_input("/group create"), Command::Help);
    }

    #[test]
    fn enter_sends_plain_text_and_clears_input() {
        let mut app = App::new(PeerId::random(), "me".to_string());
        app.textarea.insert_str("hello");
        assert_eq!(
            handle_key(&mut app, key(KeyCode::Enter, KeyModifiers::NONE)),
            InputResult::Action(Action::SendChat("hello".to_string()))
        );
        assert_eq!(app.textarea_text(), "");
        assert_eq!(app.active_conversation, GENERAL);
    }

    #[test]
    fn shift_enter_inserts_newline() {
        let mut app = App::new(PeerId::random(), "me".to_string());
        app.textarea.insert_str("first");
        handle_key(&mut app, key(KeyCode::Enter, KeyModifiers::SHIFT));
        app.textarea.insert_str("second");
        assert_eq!(app.textarea_text(), "first\nsecond");
    }

    #[test]
    fn users_command_lists_and_channel_selection_activates() {
        let mut app = App::new(PeerId::random(), "me".to_string());
        app.textarea.insert_str("/users");
        assert_eq!(
            handle_key(&mut app, key(KeyCode::Enter, KeyModifiers::NONE)),
            InputResult::Action(Action::Users)
        );
        assert_eq!(app.selection, Selection::None);

        let bob = PeerId::random();
        app.add_or_update_peer(bob, "bob", true);
        let dm = format!("dm:{bob}");
        app.activate_conversation(&dm);
        app.selection = Selection::Channels;
        app.channel_selection = 1;
        assert_eq!(
            handle_key(&mut app, key(KeyCode::Enter, KeyModifiers::NONE)),
            InputResult::Redraw
        );
        assert_eq!(app.active_conversation, dm);

        app.selection = Selection::Channels;
        app.channel_selection = 0;
        assert_eq!(
            handle_key(&mut app, key(KeyCode::Tab, KeyModifiers::NONE)),
            InputResult::Redraw
        );
        assert_eq!(app.active_conversation, GENERAL);
    }

    #[test]
    fn ctrl_c_quits() {
        let mut app = App::new(PeerId::random(), "me".to_string());
        assert_eq!(
            handle_key(&mut app, key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            InputResult::Quit
        );
    }
}
