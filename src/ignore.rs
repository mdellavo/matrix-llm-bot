use std::collections::HashSet;

/// The set of Matrix user IDs whose messages the bot ignores entirely, loaded
/// once from `Config::ignored_users` and never mutated at runtime — there's no
/// "ignore this user" command, only the config file. `on_room_message`
/// (`src/handler.rs`) checks this before doing anything else with an incoming
/// event: an ignored sender's message is dropped before classification,
/// logging, or any reply, so it can never end up in message history either
/// (`MessageLogger::recent`, read by the `message_log` skill tool and by
/// chat/greeting reply context) — there's nothing to filter out later because
/// it was never written in the first place.
#[derive(Debug, Default)]
pub struct IgnoredUsers(HashSet<String>);

impl IgnoredUsers {
    pub fn new(users: impl IntoIterator<Item = String>) -> Self {
        Self(users.into_iter().collect())
    }

    pub fn contains(&self, user_id: &str) -> bool {
        self.0.contains(user_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contains_matches_configured_users_only() {
        let ignored = IgnoredUsers::new(["@spammer:example.org".to_string(), "@troll:example.org".to_string()]);
        assert!(ignored.contains("@spammer:example.org"));
        assert!(ignored.contains("@troll:example.org"));
        assert!(!ignored.contains("@alice:example.org"));
    }

    #[test]
    fn empty_ignore_list_ignores_nobody() {
        let ignored = IgnoredUsers::default();
        assert!(!ignored.contains("@alice:example.org"));
    }
}
