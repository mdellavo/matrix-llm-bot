use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Default per-room greeting cooldown — ported from gordy's `GREETING_TIMEOUT`
/// (`gordy/bot.py`), which used the same 5-minute window to avoid replying to
/// every single "hi" in a busy room.
pub const DEFAULT_COOLDOWN: Duration = Duration::from_secs(5 * 60);

/// Tracks, per room, when the bot last sent a greeting reply — gordy's
/// `last_greeting` dict, ported. In-memory only (resets on restart), same as
/// every other piece of the bot's state besides the message log and the crypto
/// store.
#[derive(Debug)]
pub struct GreetingCooldown {
    cooldown: Duration,
    last_greeted: Mutex<HashMap<String, Instant>>,
}

impl GreetingCooldown {
    pub fn new(cooldown: Duration) -> Self {
        Self { cooldown, last_greeted: Mutex::new(HashMap::new()) }
    }

    /// Whether `room_id` is due for a greeting reply: true if it's never been
    /// greeted, or its last greeting was longer than `cooldown` ago. Records
    /// `room_id` as just-greeted as a side effect whenever this returns true —
    /// call this exactly once per candidate greeting message, since calling it
    /// again immediately after (e.g. to re-check the same message) would
    /// consume the cooldown a second time.
    pub fn try_greet(&self, room_id: &str) -> bool {
        let mut last_greeted = self.last_greeted.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let now = Instant::now();
        let ready = match last_greeted.get(room_id) {
            Some(last) => now.duration_since(*last) > self.cooldown,
            None => true,
        };
        if ready {
            last_greeted.insert(room_id.to_string(), now);
        }
        ready
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_greeting_in_a_room_is_always_allowed() {
        let cooldown = GreetingCooldown::new(Duration::from_secs(300));
        assert!(cooldown.try_greet("!room:example.org"));
    }

    #[test]
    fn a_second_greeting_within_the_cooldown_is_refused() {
        let cooldown = GreetingCooldown::new(Duration::from_secs(300));
        assert!(cooldown.try_greet("!room:example.org"));
        assert!(!cooldown.try_greet("!room:example.org"));
    }

    #[test]
    fn a_greeting_after_the_cooldown_elapses_is_allowed_again() {
        let cooldown = GreetingCooldown::new(Duration::from_millis(20));
        assert!(cooldown.try_greet("!room:example.org"));
        std::thread::sleep(Duration::from_millis(30));
        assert!(cooldown.try_greet("!room:example.org"));
    }

    #[test]
    fn rooms_have_independent_cooldowns() {
        let cooldown = GreetingCooldown::new(Duration::from_secs(300));
        assert!(cooldown.try_greet("!room-a:example.org"));
        assert!(cooldown.try_greet("!room-b:example.org"));
    }
}
