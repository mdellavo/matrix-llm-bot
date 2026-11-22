//! The newest log lines, kept in memory for the MCP endpoint's `recent_logs`, so a
//! problem can be looked into without shell access to wherever the bot runs.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::{Arc, Mutex};

use tracing::Subscriber;
use tracing_subscriber::field::RecordFields;
use tracing_subscriber::fmt::format::{DefaultFields, Writer};
use tracing_subscriber::fmt::{self, FormatFields, MakeWriter};
use tracing_subscriber::registry::LookupSpan;

/// The `fmt` layer that writes into `logs`, without color.
pub fn layer<S>(logs: LogBuffer) -> fmt::Layer<S, BufferFields, fmt::format::Format, LogBuffer>
where
    S: Subscriber + for<'span> LookupSpan<'span>,
{
    fmt::layer().with_ansi(false).fmt_fields(BufferFields::default()).with_writer(logs)
}

/// `DefaultFields` under a type of its own. A `fmt` layer keeps each span's formatted
/// fields in the span, keyed by the formatter's type, so two layers sharing
/// `DefaultFields` would both append to one copy: every span field would print twice,
/// and this layer would show the console layer's color codes.
#[derive(Debug, Default)]
pub struct BufferFields(DefaultFields);

impl<'writer> FormatFields<'writer> for BufferFields {
    fn format_fields<R: RecordFields>(&self, writer: Writer<'writer>, fields: R) -> std::fmt::Result {
        self.0.format_fields(writer, fields)
    }
}

/// A fixed number of the latest formatted log lines. Cloning shares the buffer.
#[derive(Debug, Clone)]
pub struct LogBuffer {
    inner: Arc<Mutex<Lines>>,
}

#[derive(Debug)]
struct Lines {
    capacity: usize,
    lines: VecDeque<String>,
}

impl LogBuffer {
    pub fn new(capacity: usize) -> Self {
        Self { inner: Arc::new(Mutex::new(Lines { capacity: capacity.max(1), lines: VecDeque::new() })) }
    }

    fn push(&self, text: &str) {
        let mut inner = self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        for line in text.lines().filter(|line| !line.trim().is_empty()) {
            if inner.lines.len() == inner.capacity {
                inner.lines.pop_front();
            }
            inner.lines.push_back(line.to_string());
        }
    }

    /// Up to `limit` of the newest lines, oldest first. With `min_level`, only lines at
    /// that level or worse; with `contains`, only lines with that text, ignoring case.
    pub fn recent(&self, limit: usize, min_level: Option<Level>, contains: Option<&str>) -> Vec<String> {
        let inner = self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let needle = contains.map(str::to_lowercase);
        let mut matched: Vec<String> = inner
            .lines
            .iter()
            .rev()
            .filter(|line| min_level.is_none_or(|level| Level::of(line).is_some_and(|found| found >= level)))
            .filter(|line| needle.as_ref().is_none_or(|needle| line.to_lowercase().contains(needle)))
            .take(limit)
            .cloned()
            .collect();
        matched.reverse();
        matched
    }
}

/// Log levels, least severe first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    pub fn parse(text: &str) -> Option<Self> {
        match text.to_ascii_lowercase().as_str() {
            "trace" => Some(Self::Trace),
            "debug" => Some(Self::Debug),
            "info" => Some(Self::Info),
            "warn" | "warning" => Some(Self::Warn),
            "error" => Some(Self::Error),
            _ => None,
        }
    }

    /// The level in a line written by the `fmt` layer without color, which puts it
    /// right after the timestamp.
    fn of(line: &str) -> Option<Self> {
        line.split_whitespace().nth(1).and_then(Self::parse)
    }
}

/// Each event is formatted whole and then written in one call, so a write is a line.
pub struct LineWriter(LogBuffer);

impl Write for LineWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.push(&String::from_utf8_lossy(bytes));
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for LogBuffer {
    type Writer = LineWriter;

    fn make_writer(&'a self) -> Self::Writer {
        LineWriter(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer(lines: &[&str]) -> LogBuffer {
        let buffer = LogBuffer::new(3);
        for line in lines {
            buffer.make_writer().write_all(format!("{line}\n").as_bytes()).unwrap();
        }
        buffer
    }

    #[test]
    fn keeps_only_the_newest_lines() {
        let buffer = buffer(&["2026-11-22T00:00:01Z  INFO a: one", "2026-11-22T00:00:02Z  INFO a: two", "2026-11-22T00:00:03Z  WARN a: three", "2026-11-22T00:00:04Z ERROR a: four"]);
        assert_eq!(buffer.recent(10, None, None).len(), 3);
        assert_eq!(buffer.recent(1, None, None), ["2026-11-22T00:00:04Z ERROR a: four"]);
    }

    /// With the console layer beside it, as in `main`, a span's fields print once and
    /// without color codes.
    #[test]
    fn span_fields_print_once_and_uncolored_beside_a_colored_layer() {
        use tracing_subscriber::layer::SubscriberExt;

        let console = LogBuffer::new(10);
        let logs = LogBuffer::new(10);
        let subscriber = tracing_subscriber::registry()
            .with(fmt::layer().with_ansi(true).with_writer(console.clone()))
            .with(layer(logs.clone()));
        tracing::subscriber::with_default(subscriber, || {
            tracing::info_span!("send", request_id = "REQ-0").in_scope(|| tracing::info!("sent"));
        });

        for (name, buffer) in [("buffer", &logs), ("console", &console)] {
            let line = buffer.recent(1, None, None).pop().unwrap();
            assert_eq!(line.matches("REQ-0").count(), 1, "{name}: {line}");
        }
        let line = logs.recent(1, None, None).pop().unwrap();
        assert!(!line.contains('\u{1b}'), "{line}");
        assert_eq!(Level::of(&line), Some(Level::Info));
    }

    #[test]
    fn filters_by_level_and_text() {
        let buffer = buffer(&["2026-11-22T00:00:02Z  INFO bot: synced", "2026-11-22T00:00:03Z  WARN bot: failed to send", "2026-11-22T00:00:04Z ERROR llm: Overloaded"]);
        assert_eq!(buffer.recent(10, Level::parse("warn"), None).len(), 2);
        assert_eq!(buffer.recent(10, None, Some("overloaded")), ["2026-11-22T00:00:04Z ERROR llm: Overloaded"]);
        assert!(buffer.recent(10, Some(Level::Error), Some("synced")).is_empty());
    }
}
