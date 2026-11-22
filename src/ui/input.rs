use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tui_textarea::Input;

use super::app::{App, Selection};
use super::complete::{unique_prefix, COMMANDS, GROUP_SUBCOMMANDS};

pub const HELP: &str = "Commands: /users [name] [message], /group create <name>, /group add <group> <user>, /group members <group>, /help, /quit. Tab completes commands and names.";

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

    // Unambiguous prefixes work even without pressing Tab: `/u` means `/users`.
    let mut words = first.splitn(2, char::is_whitespace);
    let head = words.next().unwrap_or("");
    let tail = words.next().unwrap_or("");
    let head = unique_prefix(head, COMMANDS).unwrap_or(head);
    let first = if tail.is_empty() {
        head.to_string()
    } else {
        format!("{head} {tail}")
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
        let mut parts = args.splitn(2, char::is_whitespace);
        let subcommand = parts.next().unwrap_or("");
        let detail = parts.next().unwrap_or("");
        let subcommand = unique_prefix(subcommand, GROUP_SUBCOMMANDS).unwrap_or(subcommand);
        let args = if detail.is_empty() {
            subcommand.to_string()
        } else {
            format!("{subcommand} {detail}")
        };
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
            app.begin_selection();
            app.move_selection(if key.code == KeyCode::Up { -1 } else { 1 });
            InputResult::Redraw
        }
        KeyCode::Tab => {
            // Completion wins over channel cycling; without a hint Tab behaves as before.
            if !app.accept_completion() {
                match app.selection {
                    Selection::Channels => {
                        app.activate_selected_channel();
                    }
                    Selection::Groups => {
                        app.activate_selected_group();
                    }
                    Selection::None => app.cycle_conversation(),
                }
            }
            InputResult::Redraw
        }
        KeyCode::Esc => {
            if !app.dismiss_completion() {
                app.selection = Selection::None;
            }
            InputResult::Redraw
        }
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
            // Editing always means the user is talking to the input, not the channel list.
            app.selection = Selection::None;
            app.textarea.input(Input {
                key: tui_textarea::Key::Enter,
                ctrl: false,
                alt: false,
                shift: true,
            });
            app.refresh_completion();
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
            // If the user picked a channel with ↑/↓ and starts typing, return focus to the input.
            // Otherwise the next Enter would only re-activate the highlighted channel.
            app.selection = Selection::None;
            app.textarea.input(Input::from(key));
            app.refresh_completion();
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
    fn unique_command_prefixes_are_accepted() {
        assert_eq!(parse_input("/u"), Command::Users);
        assert_eq!(parse_input("/q"), Command::Quit);
        assert_eq!(parse_input("/e"), Command::Quit);
        assert_eq!(parse_input("/g cr team"), Command::GroupCreate("team".to_string()));
        assert_eq!(
            parse_input("/users bo hi"),
            Command::Dm {
                user: "bo".to_string(),
                body: "hi".to_string()
            }
        );
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
    fn up_enters_channel_selection_from_input() {
        let mut app = App::new(PeerId::random(), "me".to_string());
        let bob = PeerId::random();
        app.add_or_update_peer(bob, "bob", true);
        assert_eq!(app.selection, Selection::None);

        assert_eq!(
            handle_key(&mut app, key(KeyCode::Up, KeyModifiers::NONE)),
            InputResult::Redraw
        );
        assert_eq!(app.selection, Selection::Channels);
        assert_eq!(app.channel_selection, 1); // wrapped from #general to the DM

        assert_eq!(
            handle_key(&mut app, key(KeyCode::Enter, KeyModifiers::NONE)),
            InputResult::Redraw
        );
        assert_eq!(app.active_conversation, format!("dm:{bob}"));
    }

    #[test]
    fn typing_after_channel_selection_sends_with_enter() {
        let mut app = App::new(PeerId::random(), "me".to_string());
        let bob = PeerId::random();
        app.add_or_update_peer(bob, "bob", true);

        assert_eq!(
            handle_key(&mut app, key(KeyCode::Up, KeyModifiers::NONE)),
            InputResult::Redraw
        );
        assert_eq!(app.selection, Selection::Channels);

        assert_eq!(
            handle_key(&mut app, key(KeyCode::Char('h'), KeyModifiers::NONE)),
            InputResult::Redraw
        );
        assert_eq!(
            handle_key(&mut app, key(KeyCode::Char('i'), KeyModifiers::NONE)),
            InputResult::Redraw
        );
        assert_eq!(app.selection, Selection::None);
        assert_eq!(app.textarea_text(), "hi");

        assert_eq!(
            handle_key(&mut app, key(KeyCode::Enter, KeyModifiers::NONE)),
            InputResult::Action(Action::SendChat("hi".to_string()))
        );
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
    fn tab_completes_commands_and_usernames() {
        let mut app = App::new(PeerId::random(), "me".to_string());
        app.add_or_update_peer(PeerId::random(), "bob", true);
        app.add_or_update_peer(PeerId::random(), "bobby", false);

        for ch in "/us".chars() {
            handle_key(&mut app, key(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        assert!(app.completion.is_some());
        assert_eq!(
            handle_key(&mut app, key(KeyCode::Tab, KeyModifiers::NONE)),
            InputResult::Redraw
        );
        assert_eq!(app.textarea_text(), "/users ");

        handle_key(&mut app, key(KeyCode::Char('b'), KeyModifiers::NONE));
        assert!(app.completion.is_some());
        assert_eq!(
            app.completion.as_ref().unwrap().candidates[0].value,
            "bob"
        );
        handle_key(&mut app, key(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.textarea_text(), "/users bob ");
        assert!(app.completion.is_none());
    }

    #[test]
    fn tab_completes_group_arguments() {
        let mut app = App::new(PeerId::random(), "me".to_string());
        app.add_or_update_peer(PeerId::random(), "bob", true);
        app.create_group("team");

        app.textarea.insert_str("/group add te");
        app.refresh_completion();
        handle_key(&mut app, key(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.textarea_text(), "/group add team ");

        app.textarea.insert_str("bo");
        app.refresh_completion();
        handle_key(&mut app, key(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.textarea_text(), "/group add team bob ");
    }

    #[test]
    fn esc_dismisses_hint_until_prefix_changes() {
        let mut app = App::new(PeerId::random(), "me".to_string());
        app.textarea.insert_str("/u");
        app.refresh_completion();
        assert!(app.completion.is_some());

        assert_eq!(
            handle_key(&mut app, key(KeyCode::Esc, KeyModifiers::NONE)),
            InputResult::Redraw
        );
        assert!(app.completion.is_none());
        app.refresh_completion();
        assert!(app.completion.is_none());

        app.textarea.insert_str("s");
        app.refresh_completion();
        assert_eq!(
            app.completion.as_ref().unwrap().candidates[0].value,
            "/users"
        );
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
