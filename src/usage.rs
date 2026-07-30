use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use threatflux_anthropic_sdk::models::Usage;
use tracing::warn;

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

/// Estimated USD cost of one completion's tokens, or `None` if `model` isn't
/// in `price_per_million_tokens`'s table. Excludes server-side tool fees (e.g.
/// web search) — see `web_search_cost_usd`, added separately in `add` since
/// it doesn't depend on the model's per-token price.
fn estimate_cost_usd(model: &str, usage: &Usage) -> Option<f64> {
    let (input_price, output_price) = price_per_million_tokens(model)?;
    Some(
        (f64::from(usage.input_tokens) / 1_000_000.0) * input_price
            + (f64::from(usage.output_tokens) / 1_000_000.0) * output_price,
    )
}

/// USD price per web search the model performs via the built-in `web_search`
/// server-side tool (`generate_chat_reply` — see `handler.rs`), independent of
/// which model made the call. Not fetched live; keep in sync with Anthropic's
/// published pricing if it changes.
const WEB_SEARCH_PRICE_PER_REQUEST: f64 = 0.01;

/// Estimated USD cost of one completion's web searches (`0.0` if it made
/// none) — see `WEB_SEARCH_PRICE_PER_REQUEST`.
fn web_search_cost_usd(usage: &Usage) -> f64 {
    let requests = usage.server_tool_use.as_ref().map_or(0, |server_tool_use| server_tool_use.web_search_requests);
    f64::from(requests) * WEB_SEARCH_PRICE_PER_REQUEST
}

/// Friendly reply text used by any call site that short-circuits a Claude call
/// because `UsageTracker::over_cost_limit()` is true — shown to the user
/// instead of silently skipping, so hitting the operator's budget cap doesn't
/// look like the bot is simply broken.
pub const COST_LIMIT_REPLY: &str =
    "The Claude API cost limit configured for this bot has been reached — ask the bot operator to raise it.";

/// Running token/request/cost total for one label (`"classify"`, or a skill name)
/// or for all calls combined.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct UsageTotals {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub request_count: u64,
    /// Sum of each recorded request's estimated token cost (`estimate_cost_usd`)
    /// plus any web search fees (`web_search_cost_usd`, always known regardless
    /// of the model). Doesn't include `unpriced_requests`' token cost, since
    /// there's no per-token price to apply to them.
    pub estimated_cost_usd: f64,
    /// Requests recorded under a model missing from `price_per_million_tokens`.
    /// Their tokens (and any web search fees) are still counted above, but
    /// their token cost is left out of `estimated_cost_usd` rather than
    /// silently treated as free.
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
        let search_cost = web_search_cost_usd(usage);
        match estimate_cost_usd(model, usage) {
            Some(cost) => self.estimated_cost_usd += cost + search_cost,
            None => {
                self.estimated_cost_usd += search_cost;
                self.unpriced_requests += 1;
            }
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
/// invoked so far appear), sorted by label name, plus the configured cost
/// limit (if any) and whether it's currently been reached.
#[derive(Debug, Clone, Serialize)]
pub struct UsageSnapshot {
    #[serde(flatten)]
    pub overall: UsageTotals,
    pub by_label: Vec<LabeledUsage>,
    pub cost_limit_usd: Option<f64>,
    pub cost_limit_reached: bool,
}

/// Accumulates Claude API token usage (and its estimated USD cost) across every
/// completion the bot makes — the classifier (`classify_message`, labeled
/// `"classify"`), the chat-reply generator (`generate_chat_reply`, labeled
/// `"chat"`), and each skill's prompt (`skills::execute`'s `run_prompt`, labeled
/// by skill name). Anthropic bills by token, so this is the number that actually
/// answers "how much is this bot costing," which nothing else in the bot tracks.
///
/// Persisted as a JSON snapshot at `path` (see `open`/`persist`), rewritten
/// after every `record` call, so totals survive a restart instead of resetting
/// to zero — unlike most of the bot's other in-memory-only state.
///
/// `cost_limit_usd` (from `Config::cost_limit_usd`, not persisted — it's
/// re-read from config on every startup) is an operator-set hard cap: once
/// `over_cost_limit` reports true, every Claude call site checks in via it and
/// stops making API calls entirely, the same way they check `Throttle` for an
/// active rate-limit cooldown.
#[derive(Debug)]
pub struct UsageTracker {
    path: PathBuf,
    cost_limit_usd: Option<f64>,
    inner: Mutex<Inner>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Inner {
    overall: UsageTotals,
    by_label: HashMap<String, UsageTotals>,
}

impl UsageTracker {
    /// Loads previously persisted totals from `path` (a JSON snapshot rewritten
    /// after every `record` call — see `persist`), so usage/cost tracking
    /// survives a restart instead of resetting to zero. A missing file (first
    /// run) starts from zero; a present-but-unparseable one (e.g. left over
    /// from an incompatible older version) is logged and also starts from
    /// zero, rather than failing the whole bot's startup over what is
    /// ultimately just a cost estimate.
    pub fn open(path: &Path, cost_limit_usd: Option<f64>) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create usage state dir at {}", parent.display()))?;
        }

        let inner = match std::fs::read_to_string(path) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_else(|err| {
                warn!(?err, path = %path.display(), "failed to parse persisted usage state; starting from zero");
                Inner::default()
            }),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Inner::default(),
            Err(err) => {
                return Err(err).with_context(|| format!("failed to read usage state file at {}", path.display()));
            }
        };

        Ok(Self { path: path.to_path_buf(), cost_limit_usd, inner: Mutex::new(inner) })
    }

    /// Whether cumulative estimated cost has reached `cost_limit_usd` — always
    /// `false` when no limit is configured. Checked by every Claude call site
    /// before firing a request (see the type docs above), so exceeding the
    /// operator's budget cap stops further API calls rather than just being
    /// visible after the fact.
    pub fn over_cost_limit(&self) -> bool {
        let Some(limit) = self.cost_limit_usd else {
            return false;
        };
        let inner = self.inner.lock().expect("usage tracker mutex poisoned");
        inner.overall.estimated_cost_usd >= limit
    }

    /// Records one completed Claude API call's token usage (and estimated cost —
    /// see `estimate_cost_usd`) under `label`, then persists the updated totals
    /// to `path` (see `persist`). `model` is whichever model this particular
    /// call actually used — a skill's own `model:` override if it has one, not
    /// necessarily `classify::MODEL` — since cost depends on it.
    pub fn record(&self, label: &str, model: &str, usage: &Usage) {
        let mut inner = self.inner.lock().expect("usage tracker mutex poisoned");
        inner.overall.add(model, usage);
        inner.by_label.entry(label.to_string()).or_default().add(model, usage);

        if let Err(err) = self.persist(&inner) {
            warn!(?err, path = %self.path.display(), "failed to persist usage state");
        }
    }

    /// Overwrites `path` with the current snapshot. A plain overwrite rather
    /// than the write-temp-then-rename `MessageLogger`/`bot.rs` session file use
    /// — a partial write here only costs an approximate usage/cost estimate,
    /// not state the bot depends on for correctness.
    fn persist(&self, inner: &Inner) -> Result<()> {
        let raw = serde_json::to_string_pretty(inner).context("failed to serialize usage state")?;
        std::fs::write(&self.path, raw).with_context(|| format!("failed to write usage state file at {}", self.path.display()))
    }

    pub fn snapshot(&self) -> UsageSnapshot {
        let inner = self.inner.lock().expect("usage tracker mutex poisoned");
        let mut by_label: Vec<LabeledUsage> = inner
            .by_label
            .iter()
            .map(|(label, totals)| LabeledUsage { label: label.clone(), totals: *totals })
            .collect();
        by_label.sort_by(|a, b| a.label.cmp(&b.label));
        let cost_limit_reached = self.cost_limit_usd.is_some_and(|limit| inner.overall.estimated_cost_usd >= limit);
        UsageSnapshot { overall: inner.overall, by_label, cost_limit_usd: self.cost_limit_usd, cost_limit_reached }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A path to a not-yet-existing file under a fresh temp directory, so each
    /// test gets its own isolated persisted-state file — `UsageTracker::open`
    /// creates the file (and its parent dir) on first `record`.
    fn unique_state_path(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("matrix-llm-bot-test-usage-{label}-{nanos}")).join("usage.json")
    }

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

    fn usage_with_web_searches(input_tokens: u32, output_tokens: u32, web_search_requests: u32) -> Usage {
        Usage {
            server_tool_use: Some(threatflux_anthropic_sdk::models::ServerToolUsage { web_search_requests }),
            ..usage(input_tokens, output_tokens)
        }
    }

    #[test]
    fn records_accumulate_overall_and_per_label_totals() {
        let tracker = UsageTracker::open(&unique_state_path("accumulate"), None).expect("open tracker");
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
        let tracker = UsageTracker::open(&unique_state_path("empty"), None).expect("open tracker");
        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.overall.total_tokens(), 0);
        assert_eq!(snapshot.overall.request_count, 0);
        assert!(snapshot.by_label.is_empty());
    }

    #[test]
    fn record_estimates_cost_for_a_known_model() {
        let tracker = UsageTracker::open(&unique_state_path("known-cost"), None).expect("open tracker");
        // 1,000,000 input tokens @ $1.00/M + 1,000,000 output tokens @ $5.00/M = $6.00.
        tracker.record("classify", "claude-haiku-4-5", &usage(1_000_000, 1_000_000));

        let snapshot = tracker.snapshot();
        assert!((snapshot.overall.estimated_cost_usd - 6.0).abs() < 1e-9, "{}", snapshot.overall.estimated_cost_usd);
        assert_eq!(snapshot.overall.unpriced_requests, 0);
    }

    #[test]
    fn record_adds_web_search_fees_on_top_of_token_cost() {
        let tracker = UsageTracker::open(&unique_state_path("web-search-cost"), None).expect("open tracker");
        // 100,000 input tokens @ $3.00/M + 100,000 output tokens @ $15.00/M = $1.80 for
        // claude-sonnet-5, plus 3 web searches @ $0.01 each = $0.03.
        tracker.record("chat", "claude-sonnet-5", &usage_with_web_searches(100_000, 100_000, 3));

        let snapshot = tracker.snapshot();
        assert!((snapshot.overall.estimated_cost_usd - 1.83).abs() < 1e-9, "{}", snapshot.overall.estimated_cost_usd);
        assert_eq!(snapshot.overall.unpriced_requests, 0);
    }

    #[test]
    fn record_counts_web_search_fees_even_for_an_unpriced_model() {
        let tracker = UsageTracker::open(&unique_state_path("web-search-unpriced"), None).expect("open tracker");
        tracker.record("chat", "some-future-model", &usage_with_web_searches(100, 100, 2));

        let snapshot = tracker.snapshot();
        // The model's own tokens are unpriced, but the 2 web searches @ $0.01
        // each are still a known cost and must not be dropped.
        assert!((snapshot.overall.estimated_cost_usd - 0.02).abs() < 1e-9, "{}", snapshot.overall.estimated_cost_usd);
        assert_eq!(snapshot.overall.unpriced_requests, 1);
    }

    #[test]
    fn record_flags_unpriced_requests_for_an_unknown_model_without_estimating_cost() {
        let tracker = UsageTracker::open(&unique_state_path("unpriced"), None).expect("open tracker");
        tracker.record("strain", "some-future-model", &usage(1_000_000, 1_000_000));

        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.overall.estimated_cost_usd, 0.0);
        assert_eq!(snapshot.overall.unpriced_requests, 1);
        let strain = snapshot.by_label.iter().find(|l| l.label == "strain").expect("strain label");
        assert_eq!(strain.totals.unpriced_requests, 1);
    }

    #[test]
    fn totals_survive_reopening_the_same_state_path() {
        let path = unique_state_path("persist-roundtrip");

        let tracker = UsageTracker::open(&path, None).expect("open tracker");
        tracker.record("classify", "claude-haiku-4-5", &usage(100, 20));
        tracker.record("strain", "claude-haiku-4-5", &usage(200, 40));
        drop(tracker);

        let reopened = UsageTracker::open(&path, None).expect("reopen tracker from persisted state");
        let snapshot = reopened.snapshot();
        assert_eq!(snapshot.overall.input_tokens, 300);
        assert_eq!(snapshot.overall.output_tokens, 60);
        assert_eq!(snapshot.overall.request_count, 2);
        let strain = snapshot.by_label.iter().find(|l| l.label == "strain").expect("strain label");
        assert_eq!(strain.totals.input_tokens, 200);

        let _ = std::fs::remove_dir_all(path.parent().expect("parent dir"));
    }

    #[test]
    fn open_starts_from_zero_on_malformed_state_file() {
        let path = unique_state_path("malformed");
        std::fs::create_dir_all(path.parent().expect("parent dir")).expect("create parent dir");
        std::fs::write(&path, "not valid json").expect("write malformed state file");

        let tracker = UsageTracker::open(&path, None).expect("open tracker despite malformed file");
        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.overall.request_count, 0);

        let _ = std::fs::remove_dir_all(path.parent().expect("parent dir"));
    }

    #[test]
    fn no_cost_limit_never_reports_over_limit() {
        let tracker = UsageTracker::open(&unique_state_path("no-limit"), None).expect("open tracker");
        tracker.record("classify", "claude-opus-4-8", &usage(1_000_000, 1_000_000));
        assert!(!tracker.over_cost_limit());

        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.cost_limit_usd, None);
        assert!(!snapshot.cost_limit_reached);
    }

    #[test]
    fn over_cost_limit_trips_once_estimated_cost_reaches_the_configured_limit() {
        let tracker = UsageTracker::open(&unique_state_path("cost-limit"), Some(1.0)).expect("open tracker");
        // 100,000 input tokens @ $5.00/M + 100,000 output tokens @ $25.00/M = $3.00 for claude-opus-4-8.
        assert!(!tracker.over_cost_limit(), "should not be over the limit before any calls");

        tracker.record("classify", "claude-opus-4-8", &usage(100_000, 100_000));
        assert!(tracker.over_cost_limit(), "should be over the $1.00 limit after a $3.00 call");

        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.cost_limit_usd, Some(1.0));
        assert!(snapshot.cost_limit_reached);
    }
}
