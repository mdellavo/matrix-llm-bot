use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use threatflux_anthropic_sdk::models::Usage;
use tracing::warn;

use crate::db::Database;

/// The `usage_totals` row holding the bot-wide totals. Label names come from skills
/// and fixed strings, which never start with `*`.
pub const OVERALL_LABEL: &str = "*overall";

/// USD price per million input/output tokens, for the models this bot might
/// plausibly use (the default `claude-haiku-5-5` — see `classify::MODEL` — plus
/// anything a skill's `model:` frontmatter could override it to). Not fetched
/// live; keep in sync with Anthropic's published pricing if it changes, or if a
/// skill starts using a model not listed here.
fn price_per_million_tokens(model: &str) -> Option<(f64, f64)> {
    match model {
        "claude-haiku-5-5" => Some((0.10, 0.50)),
        "claude-haiku-4-5" => Some((1.00, 5.00)),
        "claude-sonnet-5-5" | "claude-sonnet-5" => Some((2.00, 10.00)),
        "claude-sonnet-4-6" => Some((3.00, 15.00)),
        "claude-opus-5-5" => Some((4.00, 20.00)),
        "claude-opus-5" | "claude-opus-4-8" | "claude-opus-4-7" | "claude-opus-4-6" => Some((5.00, 25.00)),
        "claude-fable-5-1" | "claude-fable-5" | "claude-mythos-5-1" | "claude-mythos-5" => Some((10.00, 50.00)),
        _ => None,
    }
}

/// What a 5-minute cache write costs relative to normal input, on every model.
const CACHE_WRITE_MULTIPLIER: f64 = 1.25;

/// USD price per million tokens read from the prompt cache. Usually a tenth of the
/// input price, but the newest models are set separately.
fn cache_read_price(model: &str, input_price: f64) -> f64 {
    match model {
        "claude-opus-5-5" => 0.20,
        "claude-sonnet-5-5" => 0.10,
        "claude-fable-5-1" | "claude-mythos-5-1" => 0.25,
        _ => input_price * 0.1,
    }
}

fn per_million(tokens: u32, price: f64) -> f64 {
    (f64::from(tokens) / 1_000_000.0) * price
}

/// Estimated USD cost of one completion's tokens, or `None` if `model` isn't
/// in `price_per_million_tokens`'s table. Excludes server-side tool fees (e.g.
/// web search) — added separately in `UsageTotals::add` since they don't
/// depend on the model's per-token price.
fn estimate_cost_usd(model: &str, usage: &Usage) -> Option<f64> {
    let (input_price, output_price) = price_per_million_tokens(model)?;
    Some(
        per_million(usage.input_tokens, input_price)
            + per_million(usage.output_tokens, output_price)
            + per_million(usage.cache_read_input_tokens, cache_read_price(model, input_price))
            + per_million(usage.cache_creation_input_tokens, input_price * CACHE_WRITE_MULTIPLIER),
    )
}

/// What caching changed about one completion's cost compared to sending those tokens
/// normally: reads save money and writes cost a little extra, so this can be negative.
fn cache_savings_usd(model: &str, usage: &Usage) -> Option<f64> {
    let (input_price, _) = price_per_million_tokens(model)?;
    Some(
        per_million(usage.cache_read_input_tokens, input_price - cache_read_price(model, input_price))
            - per_million(usage.cache_creation_input_tokens, input_price * (CACHE_WRITE_MULTIPLIER - 1.0)),
    )
}

/// USD price per web search the model performs via the built-in `web_search`
/// server-side tool (chat replies and some skills), independent of
/// which model made the call. Not fetched live; keep in sync with Anthropic's
/// published pricing if it changes.
const WEB_SEARCH_PRICE_PER_REQUEST: f64 = 0.01;

/// Number of web searches the model performed for one completion (`0` if none).
fn web_search_request_count(usage: &Usage) -> u32 {
    usage.server_tool_use.as_ref().map_or(0, |server_tool_use| server_tool_use.web_search_requests)
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
    /// plus any web search fees (`web_search_requests` × `WEB_SEARCH_PRICE_PER_REQUEST`,
    /// always known regardless of the model). Doesn't include `unpriced_requests`'
    /// token cost, since there's no per-token price to apply to them.
    pub estimated_cost_usd: f64,
    /// Requests recorded under a model missing from `price_per_million_tokens`.
    /// Their tokens (and any web search fees) are still counted above, but
    /// their token cost is left out of `estimated_cost_usd` rather than
    /// silently treated as free.
    pub unpriced_requests: u64,
    /// Web searches the model performed via the built-in `web_search`
    /// server-side tool — see `web_search_request_count`. `#[serde(default)]`
    /// so a state file persisted before this field existed still parses.
    #[serde(default)]
    pub web_search_requests: u64,
    /// Portion of `estimated_cost_usd` attributable to `web_search_requests`
    /// (`web_search_requests` × `WEB_SEARCH_PRICE_PER_REQUEST`), broken out
    /// separately since it isn't a token cost. `#[serde(default)]` — see
    /// `web_search_requests`.
    #[serde(default)]
    pub web_search_cost_usd: f64,
    /// Input tokens served from the prompt cache. Not included in `input_tokens`, which
    /// the API reports as only what wasn't cached.
    #[serde(default)]
    pub cache_read_tokens: u64,
    /// Input tokens written to the prompt cache, also not in `input_tokens`.
    #[serde(default)]
    pub cache_write_tokens: u64,
    /// Net saving from caching: see `cache_savings_usd`.
    #[serde(default)]
    pub cache_saved_usd: f64,
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
        self.cache_read_tokens += u64::from(usage.cache_read_input_tokens);
        self.cache_write_tokens += u64::from(usage.cache_creation_input_tokens);
        self.cache_saved_usd += cache_savings_usd(model, usage).unwrap_or(0.0);

        let web_search_requests = web_search_request_count(usage);
        let search_cost = f64::from(web_search_requests) * WEB_SEARCH_PRICE_PER_REQUEST;
        self.web_search_requests += u64::from(web_search_requests);
        self.web_search_cost_usd += search_cost;

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
/// `"classify"`), chat replies, greetings, and unprompted comments
/// (`generate_grounded_reply`, labeled `"chat"`, `"greeting"`, and `"interjection"`),
/// and each skill's prompt (`skills::execute`, labeled by skill name). Anthropic bills by token, so this is the number that actually
/// answers "how much is this bot costing," which nothing else in the bot tracks.
///
/// Totals live in memory and are written to the database on every `record` call,
/// so they survive a restart instead of resetting to zero.
///
/// `cost_limit_usd` (from `Config::cost_limit_usd`, not persisted — it's
/// re-read from config on every startup) is an operator-set hard cap: once
/// `over_cost_limit` reports true, every Claude call site checks in via it and
/// stops making API calls entirely, the same way they check `Throttle` for an
/// active rate-limit cooldown.
#[derive(Debug)]
pub struct UsageTracker {
    database: Arc<Database>,
    cost_limit_usd: Option<f64>,
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    overall: UsageTotals,
    by_label: HashMap<String, UsageTotals>,
}

impl UsageTracker {
    /// Loads the totals saved in the database, or starts from zero on a first run.
    pub fn open(database: Arc<Database>, cost_limit_usd: Option<f64>) -> Result<Self> {
        let rows = database.with(load_totals).context("failed to load usage totals")?;

        let mut inner = Inner::default();
        for (label, totals) in rows {
            if label == OVERALL_LABEL {
                inner.overall = totals;
            } else {
                inner.by_label.insert(label, totals);
            }
        }

        Ok(Self { database, cost_limit_usd, inner: Mutex::new(inner) })
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
    /// see `estimate_cost_usd`) under `label`, then saves the updated totals.
    /// `model` is whichever model this particular
    /// call actually used — a skill's own `model:` override if it has one, not
    /// necessarily `classify::MODEL` — since cost depends on it.
    pub fn record(&self, label: &str, model: &str, usage: &Usage) {
        let mut inner = self.inner.lock().expect("usage tracker mutex poisoned");
        inner.overall.add(model, usage);
        inner.by_label.entry(label.to_string()).or_default().add(model, usage);

        let (overall, by_label) = (inner.overall, inner.by_label[label]);
        let saved = self.database.transaction(|transaction| {
            save_totals(transaction, OVERALL_LABEL, &overall)?;
            save_totals(transaction, label, &by_label)
        });
        if let Err(err) = saved {
            warn!(?err, "failed to persist usage totals");
        }
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

fn load_totals(connection: &Connection) -> rusqlite::Result<Vec<(String, UsageTotals)>> {
    let mut statement = connection.prepare(
        "SELECT label, input_tokens, output_tokens, request_count, estimated_cost_usd, unpriced_requests, \
         web_search_requests, web_search_cost_usd, cache_read_tokens, cache_write_tokens, cache_saved_usd \
         FROM usage_totals",
    )?;
    statement
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                UsageTotals {
                    input_tokens: row.get::<_, i64>(1)? as u64,
                    output_tokens: row.get::<_, i64>(2)? as u64,
                    request_count: row.get::<_, i64>(3)? as u64,
                    estimated_cost_usd: row.get(4)?,
                    unpriced_requests: row.get::<_, i64>(5)? as u64,
                    web_search_requests: row.get::<_, i64>(6)? as u64,
                    web_search_cost_usd: row.get(7)?,
                    cache_read_tokens: row.get::<_, i64>(8)? as u64,
                    cache_write_tokens: row.get::<_, i64>(9)? as u64,
                    cache_saved_usd: row.get(10)?,
                },
            ))
        })?
        .collect()
}

pub fn save_totals(connection: &Connection, label: &str, totals: &UsageTotals) -> Result<()> {
    connection
        .execute(
            "INSERT INTO usage_totals (label, input_tokens, output_tokens, request_count, estimated_cost_usd, \
             unpriced_requests, web_search_requests, web_search_cost_usd, cache_read_tokens, cache_write_tokens, \
             cache_saved_usd) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11) \
             ON CONFLICT (label) DO UPDATE SET input_tokens = excluded.input_tokens, \
             output_tokens = excluded.output_tokens, request_count = excluded.request_count, \
             estimated_cost_usd = excluded.estimated_cost_usd, unpriced_requests = excluded.unpriced_requests, \
             web_search_requests = excluded.web_search_requests, web_search_cost_usd = excluded.web_search_cost_usd, \
             cache_read_tokens = excluded.cache_read_tokens, cache_write_tokens = excluded.cache_write_tokens, \
             cache_saved_usd = excluded.cache_saved_usd",
            params![
                label,
                totals.input_tokens as i64,
                totals.output_tokens as i64,
                totals.request_count as i64,
                totals.estimated_cost_usd,
                totals.unpriced_requests as i64,
                totals.web_search_requests as i64,
                totals.web_search_cost_usd,
                totals.cache_read_tokens as i64,
                totals.cache_write_tokens as i64,
                totals.cache_saved_usd,
            ],
        )
        .context("failed to save usage totals")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db() -> Arc<Database> {
        Arc::new(Database::open_in_memory())
    }

    #[test]
    fn prices_cache_reads_and_writes_and_tracks_the_saving() {
        let tracker = UsageTracker::open(test_db(), None).unwrap();
        let mut cached = usage(1_000_000, 0);
        cached.cache_read_input_tokens = 1_000_000;
        cached.cache_creation_input_tokens = 1_000_000;
        tracker.record("chat", "claude-opus-5-5", &cached);

        let overall = tracker.snapshot().overall;
        // $4 input + $0.20 read + $5 write.
        assert!((overall.estimated_cost_usd - 9.20).abs() < 1e-9, "{}", overall.estimated_cost_usd);
        // $3.80 saved on the read, less the $1 write premium.
        assert!((overall.cache_saved_usd - 2.80).abs() < 1e-9, "{}", overall.cache_saved_usd);
        assert_eq!((overall.input_tokens, overall.cache_read_tokens, overall.cache_write_tokens), (1_000_000, 1_000_000, 1_000_000));
    }

    #[test]
    fn cache_totals_survive_a_restart() {
        let database = test_db();
        let mut cached = usage(1, 1);
        cached.cache_read_input_tokens = 700;
        UsageTracker::open(Arc::clone(&database), None).unwrap().record("chat", "claude-opus-5-5", &cached);

        let reopened = UsageTracker::open(database, None).unwrap().snapshot();
        assert_eq!(reopened.overall.cache_read_tokens, 700);
        assert!(reopened.by_label[0].totals.cache_saved_usd > 0.0);
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
        let tracker = UsageTracker::open(test_db(), None).expect("open tracker");
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
        let tracker = UsageTracker::open(test_db(), None).expect("open tracker");
        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.overall.total_tokens(), 0);
        assert_eq!(snapshot.overall.request_count, 0);
        assert!(snapshot.by_label.is_empty());
    }

    #[test]
    fn record_estimates_cost_for_a_known_model() {
        let tracker = UsageTracker::open(test_db(), None).expect("open tracker");
        // 1,000,000 input tokens @ $1.00/M + 1,000,000 output tokens @ $5.00/M = $6.00.
        tracker.record("classify", "claude-haiku-4-5", &usage(1_000_000, 1_000_000));

        let snapshot = tracker.snapshot();
        assert!((snapshot.overall.estimated_cost_usd - 6.0).abs() < 1e-9, "{}", snapshot.overall.estimated_cost_usd);
        assert_eq!(snapshot.overall.unpriced_requests, 0);
    }

    #[test]
    fn every_model_the_bot_uses_has_a_price() {
        for model in [crate::classify::MODEL, "claude-sonnet-5-5", "claude-opus-5-5"] {
            assert!(price_per_million_tokens(model).is_some(), "{model}");
        }
    }

    #[test]
    fn record_adds_web_search_fees_on_top_of_token_cost() {
        let tracker = UsageTracker::open(test_db(), None).expect("open tracker");
        // 100,000 input tokens @ $3.00/M + 100,000 output tokens @ $15.00/M = $1.80 for
        // claude-sonnet-4-6, plus 3 web searches @ $0.01 each = $0.03.
        tracker.record("chat", "claude-sonnet-4-6", &usage_with_web_searches(100_000, 100_000, 3));

        let snapshot = tracker.snapshot();
        assert!((snapshot.overall.estimated_cost_usd - 1.83).abs() < 1e-9, "{}", snapshot.overall.estimated_cost_usd);
        assert_eq!(snapshot.overall.unpriced_requests, 0);
        assert_eq!(snapshot.overall.web_search_requests, 3);
        assert!((snapshot.overall.web_search_cost_usd - 0.03).abs() < 1e-9, "{}", snapshot.overall.web_search_cost_usd);
        let chat = snapshot.by_label.iter().find(|l| l.label == "chat").expect("chat label");
        assert_eq!(chat.totals.web_search_requests, 3);
    }

    #[test]
    fn record_counts_web_search_fees_even_for_an_unpriced_model() {
        let tracker = UsageTracker::open(test_db(), None).expect("open tracker");
        tracker.record("chat", "some-future-model", &usage_with_web_searches(100, 100, 2));

        let snapshot = tracker.snapshot();
        // The model's own tokens are unpriced, but the 2 web searches @ $0.01
        // each are still a known cost and must not be dropped.
        assert!((snapshot.overall.estimated_cost_usd - 0.02).abs() < 1e-9, "{}", snapshot.overall.estimated_cost_usd);
        assert_eq!(snapshot.overall.unpriced_requests, 1);
        assert_eq!(snapshot.overall.web_search_requests, 2);
        assert!((snapshot.overall.web_search_cost_usd - 0.02).abs() < 1e-9, "{}", snapshot.overall.web_search_cost_usd);
    }

    #[test]
    fn record_flags_unpriced_requests_for_an_unknown_model_without_estimating_cost() {
        let tracker = UsageTracker::open(test_db(), None).expect("open tracker");
        tracker.record("strain", "some-future-model", &usage(1_000_000, 1_000_000));

        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.overall.estimated_cost_usd, 0.0);
        assert_eq!(snapshot.overall.unpriced_requests, 1);
        let strain = snapshot.by_label.iter().find(|l| l.label == "strain").expect("strain label");
        assert_eq!(strain.totals.unpriced_requests, 1);
    }

    #[test]
    fn totals_survive_reopening_the_database() {
        let database = test_db();

        let tracker = UsageTracker::open(Arc::clone(&database), None).expect("open tracker");
        tracker.record("classify", "claude-haiku-4-5", &usage(100, 20));
        tracker.record("strain", "claude-haiku-4-5", &usage(200, 40));
        drop(tracker);

        let reopened = UsageTracker::open(Arc::clone(&database), None).expect("reopen tracker from persisted state");
        let snapshot = reopened.snapshot();
        assert_eq!(snapshot.overall.input_tokens, 300);
        assert_eq!(snapshot.overall.output_tokens, 60);
        assert_eq!(snapshot.overall.request_count, 2);
        let strain = snapshot.by_label.iter().find(|l| l.label == "strain").expect("strain label");
        assert_eq!(strain.totals.input_tokens, 200);
    }

    #[test]
    fn no_cost_limit_never_reports_over_limit() {
        let tracker = UsageTracker::open(test_db(), None).expect("open tracker");
        tracker.record("classify", "claude-opus-4-8", &usage(1_000_000, 1_000_000));
        assert!(!tracker.over_cost_limit());

        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.cost_limit_usd, None);
        assert!(!snapshot.cost_limit_reached);
    }

    #[test]
    fn over_cost_limit_trips_once_estimated_cost_reaches_the_configured_limit() {
        let tracker = UsageTracker::open(test_db(), Some(1.0)).expect("open tracker");
        // 100,000 input tokens @ $5.00/M + 100,000 output tokens @ $25.00/M = $3.00 for claude-opus-4-8.
        assert!(!tracker.over_cost_limit(), "should not be over the limit before any calls");

        tracker.record("classify", "claude-opus-4-8", &usage(100_000, 100_000));
        assert!(tracker.over_cost_limit(), "should be over the $1.00 limit after a $3.00 call");

        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.cost_limit_usd, Some(1.0));
        assert!(snapshot.cost_limit_reached);
    }
}
