use std::collections::HashMap;
use std::sync::Mutex;

use anthropic_sdk::types::Usage;
use serde::Serialize;

/// Running token/request total for one label (`"classify"`, or a skill name) or
/// for all calls combined.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct UsageTotals {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub request_count: u64,
}

impl UsageTotals {
    // Not called outside tests today — the status server's JS computes this itself
    // from the `input_tokens`/`output_tokens` already on the wire — but kept as a
    // small public convenience for any future Rust-side consumer (e.g. a log line).
    #[allow(dead_code)]
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }

    fn add(&mut self, usage: &Usage) {
        self.input_tokens += u64::from(usage.input_tokens);
        self.output_tokens += u64::from(usage.output_tokens);
        self.request_count += 1;
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

/// Accumulates Claude API token usage across every completion the bot makes —
/// the classifier (`classify_message`, labeled `"classify"`) and each skill's
/// prompt (`skills::execute`'s `run_prompt`, labeled by skill name). Anthropic
/// bills by token, so this is the number that actually answers "how much is
/// this bot costing," which nothing else in the bot tracks. In-memory only
/// (resets on restart) — same as every other piece of the bot's state besides
/// the message log and the crypto store.
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

    /// Records one completed Claude API call's token usage under `label`.
    pub fn record(&self, label: &str, usage: &Usage) {
        let mut inner = self.inner.lock().expect("usage tracker mutex poisoned");
        inner.overall.add(usage);
        inner.by_label.entry(label.to_string()).or_default().add(usage);
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
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            server_tool_use: None,
            service_tier: None,
        }
    }

    #[test]
    fn records_accumulate_overall_and_per_label_totals() {
        let tracker = UsageTracker::new();
        tracker.record("classify", &usage(100, 20));
        tracker.record("classify", &usage(50, 10));
        tracker.record("strain", &usage(200, 40));

        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.overall.input_tokens, 350);
        assert_eq!(snapshot.overall.output_tokens, 70);
        assert_eq!(snapshot.overall.request_count, 3);
        assert_eq!(snapshot.overall.total_tokens(), 420);

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
}
