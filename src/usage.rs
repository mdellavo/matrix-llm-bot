use std::collections::HashMap;
use std::sync::Mutex;

use threatflux_anthropic_sdk::models::Usage;
use serde::Serialize;

/// USD price per million input/output tokens, for the models this bot might
/// plausibly use (the default `claude-haiku-4-5` — see `classify::MODEL` — plus
/// anything a skill's `model:` frontmatter could override it to). Not fetched
/// live; keep in sync with Anthropic's published pricing if it changes, or if a
/// skill starts using a model not listed here.
fn price_per_million_tokens(model: &str) -> Option<(f64, f64)> {
    match model {
        "claude-haiku-4-5" => Some((1.00, 5.00)),
        "claude-sonnet-5" | "claude-sonnet-4-6" => Some((3.00, 15.00)),
        "claude-opus-4-8" | "claude-opus-4-7" | "claude-opus-4-6" => Some((5.00, 25.00)),
        "claude-fable-5" | "claude-mythos-5" => Some((10.00, 50.00)),
        _ => None,
    }
}

/// Estimated USD cost of one completion, or `None` if `model` isn't in
/// `price_per_million_tokens`'s table.
fn estimate_cost_usd(model: &str, usage: &Usage) -> Option<f64> {
    let (input_price, output_price) = price_per_million_tokens(model)?;
    Some(
        (f64::from(usage.input_tokens) / 1_000_000.0) * input_price
            + (f64::from(usage.output_tokens) / 1_000_000.0) * output_price,
    )
}

/// Running token/request/cost total for one label (`"classify"`, or a skill name)
/// or for all calls combined.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct UsageTotals {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub request_count: u64,
    /// Sum of each recorded request's estimated cost — see `estimate_cost_usd`.
    /// Doesn't include `unpriced_requests`' tokens, since there's no price to
    /// apply to them.
    pub estimated_cost_usd: f64,
    /// Requests recorded under a model missing from `price_per_million_tokens`.
    /// Their tokens are still counted above, but left out of
    /// `estimated_cost_usd` rather than silently treated as free.
    pub unpriced_requests: u64,
}

impl UsageTotals {
    // Not called outside tests today — the status server's JS computes this itself
    // from the `input_tokens`/`output_tokens` already on the wire — but kept as a
    // small public convenience for any future Rust-side consumer (e.g. a log line).
    #[allow(dead_code)]
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }

    fn add(&mut self, model: &str, usage: &Usage) {
        self.input_tokens += u64::from(usage.input_tokens);
        self.output_tokens += u64::from(usage.output_tokens);
        self.request_count += 1;
        match estimate_cost_usd(model, usage) {
            Some(cost) => self.estimated_cost_usd += cost,
            None => self.unpriced_requests += 1,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct LabeledUsage {
    pub label: String,
    #[serde(flatten)]
    pub totals: UsageTotals,
}

/// Point-in-time view returned by `UsageTracker::snapshot` for the status
/// server: overall totals plus a per-label breakdown (only labels actually
/// invoked so far appear), sorted by label name.
#[derive(Debug, Clone, Serialize)]
pub struct UsageSnapshot {
    #[serde(flatten)]
    pub overall: UsageTotals,
    pub by_label: Vec<LabeledUsage>,
}

/// Accumulates Claude API token usage (and its estimated USD cost) across every
/// completion the bot makes — the classifier (`classify_message`, labeled
/// `"classify"`), the chat-reply generator (`generate_chat_reply`, labeled
/// `"chat"`), and each skill's prompt (`skills::execute`'s `run_prompt`, labeled
/// by skill name). Anthropic bills by token, so this is the number that actually
/// answers "how much is this bot costing," which nothing else in the bot tracks.
/// In-memory only (resets on restart) — same as every other piece of the bot's
/// state besides the message log and the crypto store.
#[derive(Debug, Default)]
pub struct UsageTracker {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    overall: UsageTotals,
    by_label: HashMap<String, UsageTotals>,
}

impl UsageTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one completed Claude API call's token usage (and estimated cost —
    /// see `estimate_cost_usd`) under `label`. `model` is whichever model this
    /// particular call actually used — a skill's own `model:` override if it has
    /// one, not necessarily `classify::MODEL` — since cost depends on it.
    pub fn record(&self, label: &str, model: &str, usage: &Usage) {
        let mut inner = self.inner.lock().expect("usage tracker mutex poisoned");
        inner.overall.add(model, usage);
        inner.by_label.entry(label.to_string()).or_default().add(model, usage);
    }

    pub fn snapshot(&self) -> UsageSnapshot {
        let inner = self.inner.lock().expect("usage tracker mutex poisoned");
        let mut by_label: Vec<LabeledUsage> = inner
            .by_label
            .iter()
            .map(|(label, totals)| LabeledUsage { label: label.clone(), totals: *totals })
            .collect();
        by_label.sort_by(|a, b| a.label.cmp(&b.label));
        UsageSnapshot { overall: inner.overall, by_label }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input_tokens: u32, output_tokens: u32) -> Usage {
        Usage {
            input_tokens,
            output_tokens,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            cache_creation: None,
            server_tool_use: None,
            inference_geo: None,
            service_tier: None,
        }
    }

    #[test]
    fn records_accumulate_overall_and_per_label_totals() {
        let tracker = UsageTracker::new();
        tracker.record("classify", "claude-haiku-4-5", &usage(100, 20));
        tracker.record("classify", "claude-haiku-4-5", &usage(50, 10));
        tracker.record("strain", "claude-haiku-4-5", &usage(200, 40));

        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.overall.input_tokens, 350);
        assert_eq!(snapshot.overall.output_tokens, 70);
        assert_eq!(snapshot.overall.request_count, 3);
        assert_eq!(snapshot.overall.total_tokens(), 420);
        assert_eq!(snapshot.overall.unpriced_requests, 0);

        assert_eq!(snapshot.by_label.len(), 2);
        let classify = snapshot.by_label.iter().find(|l| l.label == "classify").expect("classify label");
        assert_eq!(classify.totals.input_tokens, 150);
        assert_eq!(classify.totals.output_tokens, 30);
        assert_eq!(classify.totals.request_count, 2);
        let strain = snapshot.by_label.iter().find(|l| l.label == "strain").expect("strain label");
        assert_eq!(strain.totals.input_tokens, 200);
        assert_eq!(strain.totals.request_count, 1);

        // Sorted by label name.
        assert_eq!(snapshot.by_label[0].label, "classify");
        assert_eq!(snapshot.by_label[1].label, "strain");
    }

    #[test]
    fn snapshot_of_empty_tracker_has_zeroed_totals_and_no_labels() {
        let tracker = UsageTracker::new();
        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.overall.total_tokens(), 0);
        assert_eq!(snapshot.overall.request_count, 0);
        assert!(snapshot.by_label.is_empty());
    }

    #[test]
    fn record_estimates_cost_for_a_known_model() {
        let tracker = UsageTracker::new();
        // 1,000,000 input tokens @ $1.00/M + 1,000,000 output tokens @ $5.00/M = $6.00.
        tracker.record("classify", "claude-haiku-4-5", &usage(1_000_000, 1_000_000));

        let snapshot = tracker.snapshot();
        assert!((snapshot.overall.estimated_cost_usd - 6.0).abs() < 1e-9, "{}", snapshot.overall.estimated_cost_usd);
        assert_eq!(snapshot.overall.unpriced_requests, 0);
    }

    #[test]
    fn record_flags_unpriced_requests_for_an_unknown_model_without_estimating_cost() {
        let tracker = UsageTracker::new();
        tracker.record("strain", "some-future-model", &usage(1_000_000, 1_000_000));

        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.overall.estimated_cost_usd, 0.0);
        assert_eq!(snapshot.overall.unpriced_requests, 1);
        let strain = snapshot.by_label.iter().find(|l| l.label == "strain").expect("strain label");
        assert_eq!(strain.totals.unpriced_requests, 1);
    }
}
