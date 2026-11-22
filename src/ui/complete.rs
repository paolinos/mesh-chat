use tui_textarea::{CursorMove, TextArea};

use super::app::App;

/// Top-level commands offered by the TUI. Kept here so parsing and completion agree.
pub const COMMANDS: &[&str] = &["/users", "/group", "/help", "/quit", "/exit"];
pub const GROUP_SUBCOMMANDS: &[&str] = &["create", "add", "members"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// Text inserted into the input (no trailing space).
    pub value: String,
    /// Text shown in the completion hint.
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// The part of the current token that will be replaced.
    pub prefix: String,
    /// Ordered candidates; the first one is inserted by Tab.
    pub candidates: Vec<Candidate>,
}

/// The only option starting with `prefix`, when the prefix is unambiguous.
pub fn unique_prefix<'a>(prefix: &str, options: &[&'a str]) -> Option<&'a str> {
    let mut found = options
        .iter()
        .copied()
        .filter(|option| option.starts_with(prefix));
    let first = found.next()?;
    found.next().is_none().then_some(first)
}

fn build_completion(
    prefix: &str,
    values: impl IntoIterator<Item = (String, String)>,
) -> Option<Completion> {
    let lower = prefix.to_lowercase();
    let candidates: Vec<Candidate> = values
        .into_iter()
        .filter(|(value, label)| {
            value.to_lowercase().starts_with(&lower) || label.to_lowercase().starts_with(&lower)
        })
        .map(|(value, label)| Candidate { value, label })
        .collect();

    if candidates.is_empty() {
        return None;
    }
    // An exact match with no alternatives has nothing to add.
    if candidates.len() == 1 && candidates[0].value == prefix {
        return None;
    }
    Some(Completion {
        prefix: prefix.to_string(),
        candidates,
    })
}

fn command_values() -> impl IntoIterator<Item = (String, String)> {
    COMMANDS
        .iter()
        .map(|command| ((*command).to_string(), (*command).to_string()))
}

fn subcommand_values() -> impl IntoIterator<Item = (String, String)> {
    GROUP_SUBCOMMANDS
        .iter()
        .map(|name| ((*name).to_string(), (*name).to_string()))
}

fn username_values(app: &App) -> Vec<(String, String)> {
    let mut values: Vec<(String, String)> = app
        .peers
        .iter()
        .map(|(peer_id, peer)| (peer.username.clone(), app.peer_label(peer_id)))
        .collect();
    // Sort by the name that gets inserted, not by the `name·last4` display label.
    values.sort_by(|a, b| a.0.cmp(&b.0));
    values
}

fn group_values(app: &App) -> Vec<(String, String)> {
    let mut seen = std::collections::HashSet::new();
    let mut values: Vec<(String, String)> = app
        .groups
        .iter()
        .filter_map(|group| {
            if group.name.is_empty() || !seen.insert(group.name.clone()) {
                return None;
            }
            Some((group.name.clone(), format!("🔒 {}", group.name)))
        })
        .collect();
    values.sort_by(|a, b| a.0.cmp(&b.0));
    values
}

/// Work out what the caret is currently typing. Only the first input line can hold a command;
/// later lines are the (free-form) message body.
pub fn find_completion(app: &App) -> Option<Completion> {
    let (row, col) = app.textarea.cursor();
    if row != 0 {
        return None;
    }
    let line = app.textarea.lines().first()?;
    let before: String = line.chars().take(col).collect();
    let trimmed = before.trim_start();
    if !trimmed.starts_with('/') {
        return None;
    }

    let after_space = before.chars().last().is_some_and(char::is_whitespace);
    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    let slot = if after_space {
        tokens.len()
    } else {
        tokens.len().saturating_sub(1)
    };
    let prefix = if after_space {
        ""
    } else {
        tokens.last().copied().unwrap_or("")
    };

    // Accept abbreviations here too, so `/g add` and `/group add` offer the same completions.
    let command = unique_prefix(tokens.first().copied().unwrap_or(""), COMMANDS)
        .unwrap_or_else(|| tokens.first().copied().unwrap_or(""));
    let subcommand = tokens
        .get(1)
        .and_then(|token| unique_prefix(token, GROUP_SUBCOMMANDS))
        .or_else(|| tokens.get(1).copied());

    match (slot, command, subcommand) {
        (0, _, _) => build_completion(prefix, command_values()),
        (1, "/group", _) => build_completion(prefix, subcommand_values()),
        (1, "/users", _) => build_completion(prefix, username_values(app)),
        (2, "/group", Some("add")) | (2, "/group", Some("members")) => {
            build_completion(prefix, group_values(app))
        }
        (3, "/group", Some("add")) => build_completion(prefix, username_values(app)),
        _ => None,
    }
}

/// Replace the typed prefix with the chosen candidate. A trailing space is added at the end of
/// the line so the next completion (for example the username after `/users`) appears right away.
pub fn apply_candidate(
    textarea: &mut TextArea<'_>,
    completion: &Completion,
    candidate: &Candidate,
) {
    let (row, col) = textarea.cursor();
    let prefix_len = completion.prefix.chars().count();
    if col < prefix_len {
        return;
    }
    let line_len = textarea
        .lines()
        .get(row)
        .map(|line| line.chars().count())
        .unwrap_or(0);
    let insert = if col == line_len && !candidate.value.ends_with(char::is_whitespace) {
        format!("{} ", candidate.value)
    } else {
        candidate.value.clone()
    };

    // Completions only apply on the first line, so walking back stays on this line.
    textarea.cancel_selection();
    for _ in 0..prefix_len {
        textarea.move_cursor(CursorMove::Back);
    }
    textarea.delete_str(prefix_len);
    textarea.insert_str(insert);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::app::App;
    use libp2p::PeerId;

    fn app_with_peers() -> (App<'static>, PeerId, PeerId) {
        let mut app = App::new(PeerId::random(), "me".to_string());
        let bob = PeerId::random();
        let bobby = PeerId::random();
        app.add_or_update_peer(bob, "bob", true);
        app.add_or_update_peer(bobby, "bobby", false);
        app.create_group("team");
        (app, bob, bobby)
    }

    fn type_and_complete(app: &mut App<'_>, text: &str) -> Option<Completion> {
        app.clear_textarea();
        app.textarea.insert_str(text);
        find_completion(app)
    }

    #[test]
    fn completes_commands_and_hides_exact_matches() {
        let (mut app, _, _) = app_with_peers();
        let completion = type_and_complete(&mut app, "/us").unwrap();
        assert_eq!(completion.candidates[0].value, "/users");

        assert!(type_and_complete(&mut app, "/users").is_none());
        assert!(type_and_complete(&mut app, "hello").is_none());

        let completion = type_and_complete(&mut app, "/").unwrap();
        assert_eq!(completion.candidates.len(), COMMANDS.len());
    }

    #[test]
    fn completes_command_arguments() {
        let (mut app, _, _) = app_with_peers();

        let completion = type_and_complete(&mut app, "/group ").unwrap();
        assert_eq!(completion.candidates[0].value, "create");

        let completion = type_and_complete(&mut app, "/users b").unwrap();
        assert_eq!(
            completion
                .candidates
                .iter()
                .map(|candidate| candidate.value.as_str())
                .collect::<Vec<_>>(),
            vec!["bob", "bobby"]
        );

        let completion = type_and_complete(&mut app, "/group add te").unwrap();
        assert_eq!(completion.candidates[0].value, "team");

        let completion = type_and_complete(&mut app, "/group add team b").unwrap();
        assert_eq!(completion.candidates[0].value, "bob");

        let completion = type_and_complete(&mut app, "/group members te").unwrap();
        assert_eq!(completion.candidates[0].value, "team");

        assert!(type_and_complete(&mut app, "/users bob hello").is_none());
    }

    #[test]
    fn applies_candidate_with_trailing_space() {
        let (mut app, _, _) = app_with_peers();
        let completion = type_and_complete(&mut app, "/us").unwrap();
        apply_candidate(&mut app.textarea, &completion, &completion.candidates[0]);
        assert_eq!(app.textarea_text(), "/users ");
    }

    #[test]
    fn does_not_complete_in_the_message_body() {
        let (mut app, _, _) = app_with_peers();
        app.textarea.insert_str("/users\n/bo");
        app.textarea.move_cursor(CursorMove::Bottom);
        assert!(find_completion(&app).is_none());
    }
}
