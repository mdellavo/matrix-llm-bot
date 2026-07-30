use std::sync::Mutex;
use std::time::{Duration, Instant};

use threatflux_anthropic_sdk::AnthropicError;
use tracing::warn;

/// A circuit breaker for outgoing Claude API calls: once a request comes back
/// rate-limited (HTTP 429 / `AnthropicError::RateLimit`), every call site that
/// checks in via `remaining` skips its own Claude call entirely for a cooldown
/// window, rather than firing more requests into the same limit.
///
/// This sits above (and is deliberately separate from) the SDK's own
/// per-request retry/backoff and proactive RPS limiter
/// (`threatflux_anthropic_sdk::Config::max_retries`/`rate_limit_rps`) — that
/// layer smooths out a single request's transient failures, but by the time an
/// error actually reaches this bot, the SDK's own retry budget has already been
/// exhausted. This layer instead stops *new, unrelated* requests — a different
/// room's classification, the next chat reply, a skill invocation — from being
/// fired at all while the account is known to be rate-limited.
///
/// Shared across every call site via one `Arc<Throttle>` in `HandlerContext` —
/// the rate limit is per-API-key, not per-room or per-label, so a single global
/// cooldown is the right scope.
#[derive(Debug)]
pub struct Throttle {
    default_backoff: Duration,
    until: Mutex<Option<Instant>>,
}

impl Throttle {
    pub fn new(default_backoff: Duration) -> Self {
        Self { default_backoff, until: Mutex::new(None) }
    }

    /// `Some(remaining)` if a prior rate-limit response means Claude calls
    /// should be skipped right now; `None` once the cooldown has elapsed.
    pub fn remaining(&self) -> Option<Duration> {
        let until = *self.until.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let until = until?;
        let now = Instant::now();
        (until > now).then(|| until - now)
    }

    /// Records a rate-limit response, pausing further Claude calls until
    /// `now + default_backoff` (the SDK's `AnthropicError` doesn't carry the
    /// real `Retry-After` value through to us once its own retries are
    /// exhausted — see the module docs — so a fixed cooldown is the best we
    /// can do). Repeated calls while already throttled push the deadline
    /// further out, since each is computed from its own "now"; the `>`
    /// comparison only guards against the deadline ever moving *backward* —
    /// two calls actually completing out of order (a slower request's error
    /// handler running after a faster, later request's) must not let the
    /// slower one's now-stale, earlier-computed deadline win.
    pub fn note_rate_limited(&self) {
        let new_until = Instant::now() + self.default_backoff;
        let mut until = self.until.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if until.is_none_or(|current| new_until > current) {
            *until = Some(new_until);
            warn!(backoff = ?self.default_backoff, "Claude API rate limit hit; throttling further requests");
        }
    }
}

/// Whether `err` represents a rate-limit response — either the SDK's dedicated
/// `RateLimit` variant, or a plain HTTP 429 surfaced as `AnthropicError::Api`.
pub fn is_rate_limited(err: &AnthropicError) -> bool {
    matches!(err, AnthropicError::RateLimit(_)) || err.status_code() == Some(429)
}

/// Friendly reply text used by any call site that short-circuits a Claude call
/// because `Throttle::remaining()` is `Some` — shown to the user instead of
/// silently skipping, so a rate-limited stretch doesn't look like the bot is
/// simply broken.
pub const RATE_LIMITED_REPLY: &str = "I'm getting rate-limited by the Claude API right now — try again in a bit.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remaining_is_none_before_any_rate_limit() {
        let throttle = Throttle::new(Duration::from_secs(30));
        assert!(throttle.remaining().is_none());
    }

    #[test]
    fn note_rate_limited_starts_a_cooldown() {
        let throttle = Throttle::new(Duration::from_secs(30));
        throttle.note_rate_limited();

        let remaining = throttle.remaining().expect("should be throttled immediately after noting a rate limit");
        assert!(remaining <= Duration::from_secs(30));
        assert!(remaining > Duration::from_secs(29), "{remaining:?}");
    }

    #[test]
    fn note_rate_limited_extends_an_already_active_cooldown() {
        let throttle = Throttle::new(Duration::from_millis(50));
        throttle.note_rate_limited();

        std::thread::sleep(Duration::from_millis(20));
        let decayed = throttle.remaining().expect("still throttled");
        assert!(decayed < Duration::from_millis(50), "remaining should have decayed after sleeping: {decayed:?}");

        // A fresh rate-limit response pushes the deadline back out to
        // `now + default_backoff`, rather than leaving the older, already
        // partially-elapsed deadline in place.
        throttle.note_rate_limited();
        let extended = throttle.remaining().expect("still throttled");
        assert!(extended > decayed, "a fresh rate-limit response should push the cooldown back out: {extended:?} vs {decayed:?}");
    }

    #[test]
    fn remaining_is_none_after_the_cooldown_elapses() {
        let throttle = Throttle::new(Duration::from_millis(10));
        throttle.note_rate_limited();
        std::thread::sleep(Duration::from_millis(25));
        assert!(throttle.remaining().is_none());
    }

    #[test]
    fn is_rate_limited_matches_rate_limit_variant_and_http_429() {
        assert!(is_rate_limited(&AnthropicError::rate_limit("slow down")));
        assert!(is_rate_limited(&AnthropicError::api_error(429, "Too many requests".to_string(), None)));
        assert!(!is_rate_limited(&AnthropicError::api_error(500, "Server error".to_string(), None)));
        assert!(!is_rate_limited(&AnthropicError::auth("bad key")));
    }
}
