//! `!stats`: the status page's numbers, posted in chat.
//!
//! Anyone in a room can ask, so bot-wide numbers are only totals and breakdowns, and
//! names (of rooms or people) only come from the room it's asked in. The status
//! page's room list and its most active people across rooms stay on the page.

use std::time::Duration;

use crate::stats::{BotStats, Count, RoomStats};
use crate::usage::UsageSnapshot;

/// Entries in each breakdown line.
const BREAKDOWN_ENTRIES: usize = 5;
/// Labels in the cost line.
const COST_ENTRIES: usize = 4;

pub struct StatsInput<'a> {
    pub bot_name: &'a str,
    pub uptime: Duration,
    /// Time left in a Claude rate-limit cooldown.
    pub rate_limited: Option<Duration>,
    pub all: &'a BotStats,
    pub room: &'a RoomStats,
    pub usage: &'a UsageSnapshot,
}

pub fn reply(input: &StatsInput<'_>) -> String {
    let StatsInput { bot_name, uptime, rate_limited, all, room, usage } = *input;
    let totals = &all.totals;
    let mut lines = vec![format!(
        "**{bot_name}'s stats** · matrix-llm-bot {} · up {}",
        env!("CARGO_PKG_VERSION"),
        duration(uptime)
    )];

    let mut everywhere = vec![
        format!(
            "{} ({} in the last day, {} in the last week) from {}",
            plural(totals.messages, "message"),
            thousands(totals.messages_24h),
            thousands(totals.messages_7d),
            plural(totals.people, "person")
        ),
        plural(totals.replies, "reply"),
        plural(totals.threads, "thread"),
        plural(totals.images, "image"),
        plural(totals.links, "link"),
        plural(totals.reactions, "reaction"),
        plural(totals.facts, "saved fact"),
    ];
    if totals.private_conversations > 0 {
        everywhere.push(plural(totals.private_conversations, "private chat"));
    }
    lines.push(format!("**Everywhere:** {}", everywhere.join(" · ")));

    if room.messages > 0 {
        lines.push(format!(
            "**This room:** {} ({} in the last day, {} in the last week) from {} · {} · {}",
            plural(room.messages, "message"),
            thousands(room.messages_24h),
            thousands(room.messages_7d),
            plural(room.people, "person"),
            plural(room.replies, "reply"),
            plural(room.threads, "thread"),
        ));
        let people: Vec<String> =
            room.top_people.iter().map(|person| format!("{} {}", person.name, thousands(person.messages))).collect();
        if !people.is_empty() {
            lines.push(format!("**Most active here:** {}", people.join(" · ")));
        }
    } else {
        lines.push("**This room:** nothing logged yet".to_string());
    }

    if let Some(line) = breakdown("Replies by kind", &all.reply_kinds) {
        lines.push(line);
    }
    if let Some(line) = breakdown("Messages by intent", &all.intents) {
        lines.push(line);
    }
    if let Some(line) = shares("Mood", &all.sentiments) {
        lines.push(line);
    }
    if let Some(line) = breakdown("Top reactions", &all.reactions) {
        lines.push(line);
    }

    lines.push(claude_line(usage));
    let mut costs: Vec<_> = usage.by_label.iter().filter(|label| label.totals.estimated_cost_usd >= 0.005).collect();
    costs.sort_by(|a, b| b.totals.estimated_cost_usd.total_cmp(&a.totals.estimated_cost_usd));
    if !costs.is_empty() {
        let costs: Vec<String> = costs
            .iter()
            .take(COST_ENTRIES)
            .map(|label| format!("{} {}", label.label, money(label.totals.estimated_cost_usd)))
            .collect();
        lines.push(format!("**Biggest costs:** {}", costs.join(" · ")));
    }
    if usage.cost_limit_reached {
        lines.push("**The cost limit has been reached, so Claude calls are paused.**".to_string());
    }
    if let Some(remaining) = rate_limited {
        lines.push(format!("**Rate limited by Claude for another {}.**", duration(remaining)));
    }

    lines.join("\n")
}

fn claude_line(usage: &UsageSnapshot) -> String {
    let overall = &usage.overall;
    let spent = match usage.cost_limit_usd {
        Some(limit) => format!("{} of a {} limit", money(overall.estimated_cost_usd), money(limit)),
        None => money(overall.estimated_cost_usd),
    };
    // The API counts cached input separately from `input_tokens`.
    let input = overall.input_tokens + overall.cache_read_tokens + overall.cache_write_tokens;
    let mut parts = vec![
        format!("{spent} spent on {}", plural(overall.request_count, "request")),
        format!("{} tokens in, {} out", compact(input), compact(overall.output_tokens)),
    ];
    if overall.cache_read_tokens > 0 {
        let share = overall.cache_read_tokens as f64 / input as f64 * 100.0;
        parts.push(format!("{share:.0}% of input from cache, saving {}", money(overall.cache_saved_usd)));
    }
    if overall.web_search_requests > 0 {
        parts.push(plural(overall.web_search_requests, "web search"));
    }
    format!("**Claude:** {}", parts.join(" · "))
}

fn breakdown(label: &str, counts: &[Count]) -> Option<String> {
    let entries: Vec<String> =
        counts.iter().take(BREAKDOWN_ENTRIES).map(|count| format!("{} {}", count.name, thousands(count.count))).collect();
    (!entries.is_empty()).then(|| format!("**{label}:** {}", entries.join(" · ")))
}

/// Each entry as a share of the whole.
fn shares(label: &str, counts: &[Count]) -> Option<String> {
    let total: u64 = counts.iter().map(|count| count.count).sum();
    if total == 0 {
        return None;
    }
    let entries: Vec<String> = counts
        .iter()
        .take(BREAKDOWN_ENTRIES)
        .map(|count| format!("{} {:.0}%", count.name, count.count as f64 / total as f64 * 100.0))
        .collect();
    Some(format!("**{label}:** {}", entries.join(" · ")))
}

fn plural(count: u64, noun: &str) -> String {
    let noun = match (count, noun) {
        (1, noun) => noun.to_string(),
        (_, "person") => "people".to_string(),
        (_, "reply") => "replies".to_string(),
        (_, "web search") => "web searches".to_string(),
        (_, noun) => format!("{noun}s"),
    };
    format!("{} {noun}", thousands(count))
}

fn thousands(number: u64) -> String {
    let digits = number.to_string();
    let mut grouped = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

/// A large count at a glance: `12.3M`, `456K`.
fn compact(number: u64) -> String {
    match number {
        0..1_000 => number.to_string(),
        1_000..1_000_000 => format!("{:.0}K", number as f64 / 1e3),
        1_000_000..1_000_000_000 => format!("{:.1}M", number as f64 / 1e6),
        _ => format!("{:.1}B", number as f64 / 1e9),
    }
}

fn money(usd: f64) -> String {
    format!("${usd:.2}")
}

/// The two largest units: `3d 4h`, `5h 12m`, `12m`, `40s`.
fn duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    let (days, hours, minutes) = (seconds / 86_400, seconds / 3_600 % 24, seconds / 60 % 60);
    match (days, hours, minutes) {
        (0, 0, 0) => format!("{seconds}s"),
        (0, 0, minutes) => format!("{minutes}m"),
        (0, hours, minutes) => format!("{hours}h {minutes}m"),
        (days, hours, _) => format!("{days}d {hours}h"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::{PersonActivity, Totals};
    use crate::usage::{LabeledUsage, UsageTotals};

    fn count(name: &str, count: u64) -> Count {
        Count { name: name.to_string(), count }
    }

    fn all() -> BotStats {
        BotStats {
            totals: Totals {
                messages: 12_345,
                messages_24h: 210,
                messages_7d: 1_234,
                replies: 1_020,
                images: 312,
                links: 140,
                threads: 88,
                people: 42,
                reactions: 1,
                facts: 37,
                private_conversations: 3,
                ..Totals::default()
            },
            daily: Vec::new(),
            rooms: Vec::new(),
            people: vec![PersonActivity { name: "someone elsewhere".to_string(), messages: 9_000, replies: 0 }],
            reply_kinds: vec![count("chat", 800), count("rt", 12)],
            intents: vec![count("chitchat", 9_000), count("question", 3_000)],
            sentiments: vec![count("neutral", 60), count("positive", 25), count("negative", 15)],
            reactions: vec![count("😂", 120)],
            database_bytes: 0,
        }
    }

    fn usage() -> UsageSnapshot {
        let totals = |cost: f64| UsageTotals { estimated_cost_usd: cost, ..UsageTotals::default() };
        UsageSnapshot {
            overall: UsageTotals {
                input_tokens: 2_000_000,
                output_tokens: 456_000,
                request_count: 5_432,
                estimated_cost_usd: 12.345,
                cache_read_tokens: 6_000_000,
                cache_saved_usd: 3.21,
                web_search_requests: 120,
                ..UsageTotals::default()
            },
            by_label: vec![
                LabeledUsage { label: "classify".to_string(), totals: totals(2.2) },
                LabeledUsage { label: "chat".to_string(), totals: totals(8.1) },
                LabeledUsage { label: "pp".to_string(), totals: totals(0.0) },
            ],
            cost_limit_usd: Some(50.0),
            cost_limit_reached: false,
        }
    }

    #[test]
    fn posts_totals_this_room_and_claude_usage() {
        let room = RoomStats {
            messages: 4_321,
            messages_24h: 80,
            messages_7d: 600,
            replies: 400,
            people: 12,
            threads: 1,
            first_message_ms: None,
            top_people: vec![
                PersonActivity { name: "alice".to_string(), messages: 1_234, replies: 50 },
                PersonActivity { name: "bob".to_string(), messages: 999, replies: 10 },
            ],
        };
        let (all, usage) = (all(), usage());
        let text = reply(&StatsInput {
            bot_name: "gordy",
            uptime: Duration::from_secs(3 * 3_600 + 12 * 60),
            rate_limited: None,
            all: &all,
            room: &room,
            usage: &usage,
        });
        let expected = format!(
            "**gordy's stats** · matrix-llm-bot {} · up 3h 12m\n\
             **Everywhere:** 12,345 messages (210 in the last day, 1,234 in the last week) from 42 people · 1,020 replies · 88 threads · 312 images · 140 links · 1 reaction · 37 saved facts · 3 private chats\n\
             **This room:** 4,321 messages (80 in the last day, 600 in the last week) from 12 people · 400 replies · 1 thread\n\
             **Most active here:** alice 1,234 · bob 999\n\
             **Replies by kind:** chat 800 · rt 12\n\
             **Messages by intent:** chitchat 9,000 · question 3,000\n\
             **Mood:** neutral 60% · positive 25% · negative 15%\n\
             **Top reactions:** 😂 120\n\
             **Claude:** $12.35 of a $50.00 limit spent on 5,432 requests · 8.0M tokens in, 456K out · 75% of input from cache, saving $3.21 · 120 web searches\n\
             **Biggest costs:** chat $8.10 · classify $2.20",
            env!("CARGO_PKG_VERSION")
        );
        assert_eq!(text, expected);
        // Who's active in other rooms stays on the status page.
        assert!(!text.contains("someone elsewhere"));
    }

    #[test]
    fn says_when_the_room_is_empty_or_claude_is_paused() {
        let (all, mut usage) = (all(), usage());
        usage.cost_limit_reached = true;
        let text = reply(&StatsInput {
            bot_name: "gordy",
            uptime: Duration::from_secs(40),
            rate_limited: Some(Duration::from_secs(90)),
            all: &all,
            room: &RoomStats::default(),
            usage: &usage,
        });
        assert!(text.contains(" · up 40s\n"), "{text}");
        assert!(text.contains("\n**This room:** nothing logged yet\n"), "{text}");
        assert!(!text.contains("Most active here"), "{text}");
        assert!(text.ends_with("**The cost limit has been reached, so Claude calls are paused.**\n**Rate limited by Claude for another 1m.**"), "{text}");
    }

    #[test]
    fn formats_numbers() {
        assert_eq!([thousands(0), thousands(999), thousands(1_000), thousands(1_234_567)], ["0", "999", "1,000", "1,234,567"]);
        assert_eq!([compact(999), compact(45_600), compact(12_345_678), compact(2_500_000_000)], ["999", "46K", "12.3M", "2.5B"]);
        assert_eq!(
            [Duration::from_secs(5), Duration::from_secs(600), Duration::from_secs(7_260), Duration::from_secs(3 * 86_400 + 4 * 3_600)]
                .map(duration),
            ["5s", "10m", "2h 1m", "3d 4h"]
        );
        assert_eq!([plural(1, "person"), plural(2, "person"), plural(2, "web search"), plural(0, "reply")], ["1 person", "2 people", "2 web searches", "0 replies"]);
    }
}
