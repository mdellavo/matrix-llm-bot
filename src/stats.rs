//! Counters for the status page, worked out from the database when asked for.
//!
//! Private conversations are left out of everything here except their total, since
//! the page has no login.

use anyhow::Result;
use rusqlite::{Connection, Row, params};
use serde::Serialize;

use crate::db::Database;

/// Limits rows to rooms the status page may show.
const NOT_PRIVATE: &str = "room_id NOT IN (SELECT room_id FROM direct_rooms)";

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// Days of activity to report.
const DAILY_WINDOW_DAYS: i64 = 14;

/// Rows in each ranked list.
const TOP_LIMIT: i64 = 10;

#[derive(Debug, Default, Serialize)]
pub struct Totals {
    pub messages: u64,
    pub messages_24h: u64,
    pub messages_7d: u64,
    pub replies: u64,
    pub images: u64,
    pub links: u64,
    pub threads: u64,
    pub people: u64,
    pub reactions: u64,
    pub facts: u64,
    pub room_summaries: u64,
    pub private_conversations: u64,
    pub first_message_ms: Option<i64>,
    pub last_message_ms: Option<i64>,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct DayActivity {
    pub day: String,
    pub messages: u64,
    pub replies: u64,
}

#[derive(Debug, Serialize)]
pub struct RoomActivity {
    pub room_id: String,
    pub messages: u64,
    pub replies: u64,
    pub people: u64,
    pub threads: u64,
    pub last_message_ms: i64,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct PersonActivity {
    pub name: String,
    pub messages: u64,
    pub replies: u64,
}

/// One entry of a breakdown, such as an intent or a reaction.
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct Count {
    pub name: String,
    pub count: u64,
}

#[derive(Debug, Serialize)]
pub struct BotStats {
    pub totals: Totals,
    pub daily: Vec<DayActivity>,
    pub rooms: Vec<RoomActivity>,
    pub people: Vec<PersonActivity>,
    pub reply_kinds: Vec<Count>,
    pub intents: Vec<Count>,
    pub sentiments: Vec<Count>,
    pub reactions: Vec<Count>,
    pub database_bytes: u64,
}

fn count_of(connection: &Connection, sql: &str) -> rusqlite::Result<u64> {
    connection.query_row(sql, [], |row| row.get::<_, i64>(0)).map(|count| count.max(0).cast_unsigned())
}

fn read_count(row: &Row<'_>) -> rusqlite::Result<Count> {
    Ok(Count { name: row.get(0)?, count: row.get::<_, i64>(1)?.max(0).cast_unsigned() })
}

fn breakdown(connection: &Connection, sql: &str) -> rusqlite::Result<Vec<Count>> {
    connection.prepare(sql)?.query_map([], read_count)?.collect()
}

fn totals(connection: &Connection, now_ms: i64) -> rusqlite::Result<Totals> {
    let mut totals = connection.query_row(
        &format!(
            "SELECT COUNT(*), \
             COALESCE(SUM(origin_server_ts_ms >= ?1), 0), \
             COALESCE(SUM(origin_server_ts_ms >= ?2), 0), \
             COUNT(response), \
             COALESCE(SUM(has_image), 0), \
             COALESCE(SUM(json_array_length(links_json)), 0), \
             COUNT(DISTINCT thread_root_id), \
             COUNT(DISTINCT sender), \
             MIN(origin_server_ts_ms), \
             MAX(origin_server_ts_ms) \
             FROM messages WHERE {NOT_PRIVATE}"
        ),
        params![now_ms - DAY_MS, now_ms - 7 * DAY_MS],
        |row| {
            let number = |index| row.get::<_, i64>(index).map(|value| value.max(0).cast_unsigned());
            Ok(Totals {
                messages: number(0)?,
                messages_24h: number(1)?,
                messages_7d: number(2)?,
                replies: number(3)?,
                images: number(4)?,
                links: number(5)?,
                threads: number(6)?,
                people: number(7)?,
                first_message_ms: row.get(8)?,
                last_message_ms: row.get(9)?,
                ..Totals::default()
            })
        },
    )?;

    totals.reactions = count_of(connection, &format!("SELECT COUNT(*) FROM reactions WHERE {NOT_PRIVATE}"))?;
    totals.facts = count_of(connection, &format!("SELECT COUNT(*) FROM facts WHERE {NOT_PRIVATE}"))?;
    totals.room_summaries = count_of(connection, &format!("SELECT COUNT(*) FROM room_summaries WHERE {NOT_PRIVATE}"))?;
    totals.private_conversations = count_of(connection, "SELECT COUNT(*) FROM direct_rooms")?;
    Ok(totals)
}

fn daily(connection: &Connection, now_ms: i64) -> rusqlite::Result<Vec<DayActivity>> {
    connection
        .prepare(&format!(
            "SELECT date(origin_server_ts_ms / 1000, 'unixepoch') AS day, COUNT(*), COUNT(response) \
             FROM messages WHERE origin_server_ts_ms >= ?1 AND {NOT_PRIVATE} GROUP BY day ORDER BY day DESC"
        ))?
        .query_map([now_ms - DAILY_WINDOW_DAYS * DAY_MS], |row| {
            Ok(DayActivity {
                day: row.get(0)?,
                messages: row.get::<_, i64>(1)?.max(0).cast_unsigned(),
                replies: row.get::<_, i64>(2)?.max(0).cast_unsigned(),
            })
        })?
        .collect()
}

fn rooms(connection: &Connection) -> rusqlite::Result<Vec<RoomActivity>> {
    connection
        .prepare(&format!(
            "SELECT room_id, COUNT(*), COUNT(response), COUNT(DISTINCT sender), \
             COUNT(DISTINCT thread_root_id), MAX(origin_server_ts_ms) \
             FROM messages WHERE {NOT_PRIVATE} GROUP BY room_id ORDER BY COUNT(*) DESC"
        ))?
        .query_map([], |row| {
            let number = |index| row.get::<_, i64>(index).map(|value| value.max(0).cast_unsigned());
            Ok(RoomActivity {
                room_id: row.get(0)?,
                messages: number(1)?,
                replies: number(2)?,
                people: number(3)?,
                threads: number(4)?,
                last_message_ms: row.get(5)?,
            })
        })?
        .collect()
}

fn people(connection: &Connection) -> rusqlite::Result<Vec<PersonActivity>> {
    connection
        .prepare(&format!(
            "SELECT COALESCE(MAX(sender_name), sender), COUNT(*), COUNT(response) \
             FROM messages WHERE {NOT_PRIVATE} GROUP BY sender ORDER BY COUNT(*) DESC LIMIT ?1"
        ))?
        .query_map([TOP_LIMIT], |row| {
            Ok(PersonActivity {
                name: row.get(0)?,
                messages: row.get::<_, i64>(1)?.max(0).cast_unsigned(),
                replies: row.get::<_, i64>(2)?.max(0).cast_unsigned(),
            })
        })?
        .collect()
}

/// One room's activity, for `!stats` in that room.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RoomStats {
    pub messages: u64,
    pub messages_24h: u64,
    pub messages_7d: u64,
    pub replies: u64,
    pub people: u64,
    pub threads: u64,
    pub first_message_ms: Option<i64>,
    /// Who talks most here, most first.
    pub top_people: Vec<PersonActivity>,
}

/// Rows in `RoomStats::top_people`.
const ROOM_TOP_PEOPLE: i64 = 5;

/// Counts one room's messages. Unlike `collect`, this includes a private
/// conversation, since it's only asked for from inside the room.
pub fn room(database: &Database, room_id: &str, now_ms: i64) -> Result<RoomStats> {
    database.with(|connection| {
        let mut stats = connection.query_row(
            "SELECT COUNT(*), COALESCE(SUM(origin_server_ts_ms >= ?2), 0), COALESCE(SUM(origin_server_ts_ms >= ?3), 0), \
             COUNT(response), COUNT(DISTINCT sender), COUNT(DISTINCT thread_root_id), MIN(origin_server_ts_ms) \
             FROM messages WHERE room_id = ?1",
            params![room_id, now_ms - DAY_MS, now_ms - 7 * DAY_MS],
            |row| {
                let number = |index| row.get::<_, i64>(index).map(|value| value.max(0).cast_unsigned());
                Ok(RoomStats {
                    messages: number(0)?,
                    messages_24h: number(1)?,
                    messages_7d: number(2)?,
                    replies: number(3)?,
                    people: number(4)?,
                    threads: number(5)?,
                    first_message_ms: row.get(6)?,
                    top_people: Vec::new(),
                })
            },
        )?;
        stats.top_people = connection
            .prepare(
                "SELECT COALESCE(MAX(sender_name), sender), COUNT(*), COUNT(response) \
                 FROM messages WHERE room_id = ?1 GROUP BY sender ORDER BY COUNT(*) DESC, sender LIMIT ?2",
            )?
            .query_map(params![room_id, ROOM_TOP_PEOPLE], |row| {
                Ok(PersonActivity {
                    name: row.get(0)?,
                    messages: row.get::<_, i64>(1)?.max(0).cast_unsigned(),
                    replies: row.get::<_, i64>(2)?.max(0).cast_unsigned(),
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(stats)
    })
}

/// Counts everything shown on the status page's stats section.
pub fn collect(database: &Database, now_ms: i64) -> Result<BotStats> {
    database.with(|connection| {
        let analysis = |field: &str| {
            format!(
                "SELECT COALESCE(json_extract(analysis_json, '$.{field}'), 'unknown') AS name, COUNT(*) \
                 FROM messages WHERE {NOT_PRIVATE} GROUP BY name ORDER BY COUNT(*) DESC"
            )
        };

        Ok(BotStats {
            totals: totals(connection, now_ms)?,
            daily: daily(connection, now_ms)?,
            rooms: rooms(connection)?,
            people: people(connection)?,
            reply_kinds: breakdown(
                connection,
                &format!(
                    "SELECT response_label, COUNT(*) FROM messages WHERE response_label IS NOT NULL AND {NOT_PRIVATE} \
                     GROUP BY response_label ORDER BY COUNT(*) DESC LIMIT {TOP_LIMIT}"
                ),
            )?,
            intents: breakdown(connection, &analysis("intent"))?,
            sentiments: breakdown(connection, &analysis("sentiment"))?,
            reactions: breakdown(
                connection,
                &format!("SELECT key, COUNT(*) FROM reactions WHERE {NOT_PRIVATE} GROUP BY key ORDER BY COUNT(*) DESC LIMIT {TOP_LIMIT}"),
            )?,
            database_bytes: connection
                .query_row("SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()", [], |row| row.get::<_, i64>(0))?
                .max(0)
                .cast_unsigned(),
        })
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::classify::{Intent, MessageAnalysis, Sentiment};
    use crate::direct_rooms::DirectRooms;
    use crate::links::LinkPreview;
    use crate::message_log::{GeneratedReply, MessageLogParams, MessageLogger};

    const NOW: i64 = 1_800_000_000_000;

    struct Line<'a> {
        room_id: &'a str,
        event_id: &'a str,
        sender: &'a str,
        age_ms: i64,
        intent: Intent,
        reply: Option<&'a str>,
        thread_root_id: Option<&'a str>,
        links: &'a [LinkPreview],
    }

    fn line<'a>(room_id: &'a str, event_id: &'a str, sender: &'a str) -> Line<'a> {
        Line { room_id, event_id, sender, age_ms: 0, intent: Intent::Chitchat, reply: None, thread_root_id: None, links: &[] }
    }

    fn log(logger: &MessageLogger, line: &Line<'_>) {
        let analysis = MessageAnalysis {
            intent: line.intent,
            confidence: 0.9,
            requires_response: false,
            summary: "test".to_string(),
            sentiment: Sentiment::Neutral,
            entities: vec![],
            command: None,
        };
        let response = line.reply.map(|_| GeneratedReply::plain("hi", "chat"));
        logger
            .log(MessageLogParams {
                room_id: line.room_id,
                event_id: line.event_id,
                sender: line.sender,
                sender_name: None,
                origin_server_ts_ms: NOW - line.age_ms,
                body: "hello",
                has_image: false,
                links: line.links,
                analysis: &analysis,
                classify_prompt: None,
                classify_raw_json: None,
                response: response.as_ref(),
                response_event_id: line.reply,
                thread_root_id: line.thread_root_id,
            })
            .unwrap();
    }

    fn setup() -> (Arc<Database>, MessageLogger) {
        let database = Arc::new(Database::open_in_memory());
        (Arc::clone(&database), MessageLogger::new(database))
    }

    #[test]
    fn an_empty_database_counts_as_zero() {
        let (database, _) = setup();
        let stats = collect(&database, NOW).unwrap();
        assert_eq!(stats.totals.messages, 0);
        assert_eq!(stats.totals.last_message_ms, None);
        assert!(stats.rooms.is_empty());
    }

    #[test]
    fn counts_messages_replies_threads_and_windows() {
        let (database, logger) = setup();
        let links = [LinkPreview { url: "https://example.com".to_string(), title: None, description: None, text: None, image: false }];

        log(&logger, &Line { reply: Some("$r1"), thread_root_id: Some("$a"), intent: Intent::Question, ..line("!r", "$a", "@alice:x") });
        log(&logger, &Line { thread_root_id: Some("$a"), links: &links, ..line("!r", "$b", "@bob:x") });
        log(&logger, &Line { age_ms: 3 * DAY_MS, ..line("!r", "$c", "@alice:x") });
        log(&logger, &Line { age_ms: 30 * DAY_MS, ..line("!other", "$d", "@alice:x") });
        logger.log_reaction("!r", "$a", "bob", "👍").unwrap();

        let stats = collect(&database, NOW).unwrap();
        let totals = &stats.totals;
        assert_eq!((totals.messages, totals.messages_24h, totals.messages_7d), (4, 2, 3));
        assert_eq!((totals.replies, totals.threads, totals.people, totals.links, totals.reactions), (1, 1, 2, 1, 1));

        assert_eq!(stats.rooms[0].room_id, "!r");
        assert_eq!(stats.rooms[0].messages, 3);
        assert_eq!(stats.people[0], PersonActivity { name: "@alice:x".to_string(), messages: 3, replies: 1 });
        assert_eq!(stats.intents[0], Count { name: "chitchat".to_string(), count: 3 });
        assert_eq!(stats.reply_kinds, [Count { name: "chat".to_string(), count: 1 }]);
        assert_eq!(stats.daily.len(), 2, "the 30-day-old message is outside the window");
        assert!(stats.database_bytes > 0);
    }

    #[test]
    fn replies_from_before_event_ids_were_recorded_still_count() {
        let (database, logger) = setup();
        let analysis = MessageAnalysis::unclassified(Intent::Chitchat, "test");
        let reply = GeneratedReply::plain("hi", "chat");
        logger
            .log(MessageLogParams {
                room_id: "!r",
                event_id: "$old",
                sender: "@alice:x",
                sender_name: None,
                origin_server_ts_ms: NOW,
                body: "hello",
                has_image: false,
                links: &[],
                analysis: &analysis,
                classify_prompt: None,
                classify_raw_json: None,
                response: Some(&reply),
                response_event_id: None,
                thread_root_id: None,
            })
            .unwrap();

        assert_eq!(collect(&database, NOW).unwrap().totals.replies, 1);
    }

    #[test]
    fn private_conversations_are_only_counted_as_a_total() {
        let (database, logger) = setup();
        let direct_rooms = DirectRooms::new(Arc::clone(&database), Vec::new());
        direct_rooms.add("!private:x", "@alice:x").unwrap();

        log(&logger, &Line { reply: Some("$r"), ..line("!private:x", "$a", "@secret:x") });
        logger.log_reaction("!private:x", "$a", "secret", "🤫").unwrap();
        log(&logger, &line("!public:x", "$b", "@bob:x"));

        let stats = collect(&database, NOW).unwrap();
        assert_eq!(stats.totals.private_conversations, 1);
        assert_eq!((stats.totals.messages, stats.totals.replies, stats.totals.reactions, stats.totals.people), (1, 0, 0, 1));
        assert_eq!(stats.rooms.len(), 1);
        assert_eq!(stats.people.len(), 1);
        assert!(stats.reactions.is_empty() && stats.reply_kinds.is_empty());

        let json = serde_json::to_string(&stats).unwrap();
        assert!(!json.contains("secret") && !json.contains("!private"), "{json}");
    }

    #[test]
    fn counts_one_room_including_a_private_one() {
        let (database, logger) = setup();
        let direct_rooms = DirectRooms::new(Arc::clone(&database), Vec::new());
        direct_rooms.add("!private:x", "@alice:x").unwrap();

        log(&logger, &Line { reply: Some("$r1"), thread_root_id: Some("$a"), ..line("!r", "$a", "@alice:x") });
        log(&logger, &Line { age_ms: 3 * DAY_MS, ..line("!r", "$b", "@bob:x") });
        log(&logger, &Line { age_ms: 30 * DAY_MS, ..line("!r", "$c", "@alice:x") });
        log(&logger, &line("!other", "$d", "@carol:x"));
        log(&logger, &line("!private:x", "$e", "@alice:x"));

        let stats = room(&database, "!r", NOW).unwrap();
        assert_eq!((stats.messages, stats.messages_24h, stats.messages_7d), (3, 1, 2));
        assert_eq!((stats.replies, stats.people, stats.threads), (1, 2, 1));
        assert_eq!(stats.first_message_ms, Some(NOW - 30 * DAY_MS));
        assert_eq!(
            stats.top_people,
            [
                PersonActivity { name: "@alice:x".to_string(), messages: 2, replies: 1 },
                PersonActivity { name: "@bob:x".to_string(), messages: 1, replies: 0 },
            ]
        );

        assert_eq!(room(&database, "!private:x", NOW).unwrap().messages, 1);
        assert_eq!(room(&database, "!empty", NOW).unwrap(), RoomStats::default());
    }
}
