use std::collections::HashMap;
use std::sync::Mutex;

use threatflux_anthropic_sdk::Client as Anthropic;
use tracing::warn;

use crate::classify;
use crate::llm::{self, CompletionRequest};
use crate::message_log::{LoggedMessage, format_transcript};
use crate::throttle::Throttle;
use crate::usage::UsageTracker;

/// Older messages needed before a summary is worth a Claude call at all.
const MIN_MESSAGES_TO_SUMMARIZE: usize = 15;

/// New older messages tolerated before a cached summary is regenerated.
const REFRESH_AFTER_MESSAGES: usize = 25;

const USAGE_LABEL: &str = "room_summary";

const SYSTEM_PROMPT: &str = "You keep the long-term memory of a Matrix chat room's resident bot. Given a \
transcript, write short notes the bot can use later: ongoing topics, running jokes, what each regular \
has been up to, and anything people said they'd do or asked the bot to remember. Attribute everything to \
the person who said it. Record only what's in the transcript. At most 150 words, plain sentences.";

struct CachedSummary {
    /// The newest message this summary covers.
    through_event_id: String,
    text: String,
}

/// Per-room rolling summaries of chat older than the verbatim history window.
/// In memory only: after a restart a room's summary is rebuilt the next time the
/// bot replies there, since the message log it's derived from is on disk.
#[derive(Default)]
pub struct RoomSummaries {
    rooms: Mutex<HashMap<String, CachedSummary>>,
}

impl RoomSummaries {
    /// A summary of `older` (oldest first), refreshed only once enough new messages
    /// have aged into it. A failed refresh falls back to the previous summary, since
    /// this is optional context and must never cost the bot its reply.
    pub async fn summary_of(
        &self,
        room_id: &str,
        older: &[LoggedMessage],
        bot_name: &str,
        client: &Anthropic,
        usage_tracker: &UsageTracker,
        rate_limit: &Throttle,
    ) -> Option<String> {
        let cached = self.cached(room_id);
        let Some(newest) = older.last() else {
            return cached.map(|(_, text)| text);
        };

        let unseen = match &cached {
            Some((through, _)) => older.iter().rev().take_while(|entry| &entry.event_id != through).count(),
            None => older.len(),
        };
        if !needs_refresh(cached.is_some(), unseen, older.len()) {
            return cached.map(|(_, text)| text);
        }

        let transcript = format_transcript(older, bot_name);
        let result = llm::complete(
            client,
            &CompletionRequest {
                model: classify::MODEL,
                system: SYSTEM_PROMPT,
                user: &transcript,
                max_tokens: 1024,
                effort: None,
                web_search_max_uses: None,
                label: USAGE_LABEL,
            },
            usage_tracker,
            rate_limit,
        )
        .await;

        match result {
            Ok(text) => {
                let mut rooms = self.rooms.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                rooms.insert(
                    room_id.to_string(),
                    CachedSummary { through_event_id: newest.event_id.clone(), text: text.clone() },
                );
                Some(text)
            }
            Err(err) => {
                warn!(?err, room_id, "failed to refresh room summary");
                cached.map(|(_, text)| text)
            }
        }
    }

    fn cached(&self, room_id: &str) -> Option<(String, String)> {
        let rooms = self.rooms.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        rooms.get(room_id).map(|summary| (summary.through_event_id.clone(), summary.text.clone()))
    }
}

fn needs_refresh(has_summary: bool, unseen: usize, total: usize) -> bool {
    if has_summary { unseen >= REFRESH_AFTER_MESSAGES } else { total >= MIN_MESSAGES_TO_SUMMARIZE }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_rooms_without_enough_history_to_be_worth_a_call() {
        assert!(!needs_refresh(false, 14, 14));
        assert!(needs_refresh(false, 15, 15));
    }

    #[test]
    fn keeps_a_summary_until_enough_new_messages_arrive() {
        assert!(!needs_refresh(true, 24, 200));
        assert!(needs_refresh(true, 25, 200));
    }
}
