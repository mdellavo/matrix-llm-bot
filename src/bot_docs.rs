//! The bot's own README and changelog, read from GitHub so that answers about what it
//! can do stay current without shipping the docs in the image. The URLs are fixed, so
//! nothing here fetches anything a user supplied.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tracing::warn;

use crate::llm::LocalTools;
use crate::room_memory::MemoryTools;

const DOCS_BASE_URL: &str = "https://raw.githubusercontent.com/mdellavo/matrix-llm-bot/main";
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
const USER_AGENT: &str = concat!("matrix-llm-bot/", env!("CARGO_PKG_VERSION"));

/// How long a copy is used before checking GitHub again.
const CACHE_TTL: Duration = Duration::from_secs(60 * 60);

pub const READ_DOCS_TOOL: &str = "read_bot_docs";

pub struct BotDocs {
    client: reqwest::Client,
    base_url: String,
    /// Held across the fetch, so several questions at once only fetch once.
    cache: Mutex<HashMap<&'static str, (Instant, String)>>,
}

impl BotDocs {
    pub fn new() -> Result<Self> {
        Self::with_base_url(DOCS_BASE_URL.to_string())
    }

    fn with_base_url(base_url: String) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(FETCH_TIMEOUT)
            .user_agent(USER_AGENT)
            .build()
            .context("failed to build docs client")?;
        Ok(Self { client, base_url, cache: Mutex::new(HashMap::new()) })
    }

    async fn fetch(&self, file: &str) -> Result<String> {
        let url = format!("{}/{file}", self.base_url);
        Ok(self.client.get(url).send().await?.error_for_status()?.text().await?)
    }

    /// A document's text. An expired copy is still used if GitHub can't be reached,
    /// since slightly old docs beat none.
    async fn read(&self, file: &'static str) -> Result<String> {
        let mut cache = self.cache.lock().await;
        if let Some((fetched_at, text)) = cache.get(file)
            && fetched_at.elapsed() < CACHE_TTL
        {
            return Ok(text.clone());
        }

        match self.fetch(file).await {
            Ok(text) => {
                cache.insert(file, (Instant::now(), text.clone()));
                Ok(text)
            }
            Err(err) => match cache.get(file) {
                Some((_, text)) => {
                    warn!(?err, file, "failed to refresh docs; using the saved copy");
                    Ok(text.clone())
                }
                None => Err(err),
            },
        }
    }
}

/// The local tools offered to chat replies.
pub struct ChatTools<'a> {
    pub memory: MemoryTools<'a>,
    pub docs: &'a BotDocs,
}

impl ChatTools<'_> {
    async fn read_docs(&self, input: &Value) -> Result<String, String> {
        let file = match input["document"].as_str() {
            Some("readme") => "README.md",
            Some("changelog") => "CHANGELOG.md",
            _ => return Err("document must be readme or changelog".to_string()),
        };
        self.docs.read(file).await.map_err(|err| {
            warn!(?err, file, "failed to read docs");
            "The docs couldn't be loaded right now.".to_string()
        })
    }
}

impl LocalTools for ChatTools<'_> {
    fn definitions(&self) -> Vec<Value> {
        let mut tools = self.memory.definitions();
        tools.push(json!({
            "name": READ_DOCS_TOOL,
            "description": "Read your own documentation. Use the readme for what you can do, your commands, how \
                features work, and setup. Use the changelog for what's new or changed.",
            "input_schema": {
                "type": "object",
                "properties": {"document": {"type": "string", "enum": ["readme", "changelog"]}},
                "required": ["document"]
            }
        }));
        tools
    }

    fn run<'a>(&'a self, name: &'a str, input: &'a Value) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        if name == READ_DOCS_TOOL {
            Box::pin(self.read_docs(input))
        } else {
            self.memory.run(name, input)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::{Router, http::StatusCode, routing::get};

    use super::*;

    async fn serve(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://127.0.0.1:{port}")
    }

    #[tokio::test]
    async fn reads_a_document_once_within_the_cache_time() {
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        let app = Router::new().route(
            "/README.md",
            get(move || {
                counter.fetch_add(1, Ordering::SeqCst);
                async { "# docs" }
            }),
        );
        let docs = BotDocs::with_base_url(serve(app).await).unwrap();

        assert_eq!(docs.read("README.md").await.unwrap(), "# docs");
        assert_eq!(docs.read("README.md").await.unwrap(), "# docs");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn keeps_an_expired_copy_when_github_is_down() {
        let app = Router::new().route("/README.md", get(|| async { StatusCode::SERVICE_UNAVAILABLE }));
        let docs = BotDocs::with_base_url(serve(app).await).unwrap();
        let expired = Instant::now().checked_sub(CACHE_TTL * 2).unwrap();
        docs.cache.lock().await.insert("README.md", (expired, "old docs".to_string()));

        assert_eq!(docs.read("README.md").await.unwrap(), "old docs");
    }

    #[tokio::test]
    async fn a_failure_with_no_saved_copy_is_an_error() {
        let docs = BotDocs::with_base_url(serve(Router::new()).await).unwrap();
        assert!(docs.read("CHANGELOG.md").await.is_err());
    }

    fn chat_tools<'a>(memory: &'a crate::room_memory::RoomMemory, docs: &'a BotDocs) -> ChatTools<'a> {
        ChatTools { memory: MemoryTools { memory, room_id: "!a", told_by: "alice" }, docs }
    }

    #[tokio::test]
    async fn the_tool_reads_only_the_two_documents() {
        let app = Router::new().route("/CHANGELOG.md", get(|| async { "## 1.0" }));
        let docs = BotDocs::with_base_url(serve(app).await).unwrap();
        let memory = crate::room_memory::RoomMemory::new(Arc::new(crate::db::Database::open_in_memory()));
        let tools = chat_tools(&memory, &docs);

        assert_eq!(tools.run(READ_DOCS_TOOL, &json!({"document": "changelog"})).await.unwrap(), "## 1.0");
        assert!(tools.run(READ_DOCS_TOOL, &json!({"document": "../secrets"})).await.is_err());
        assert!(tools.run(READ_DOCS_TOOL, &json!({"document": "readme"})).await.is_err(), "not served here");
    }

    #[tokio::test]
    async fn memory_tools_still_work_alongside_it() {
        let docs = BotDocs::with_base_url("http://127.0.0.1:1".to_string()).unwrap();
        let memory = crate::room_memory::RoomMemory::new(Arc::new(crate::db::Database::open_in_memory()));
        let tools = chat_tools(&memory, &docs);

        let names: Vec<String> = tools.definitions().iter().map(|tool| tool["name"].as_str().unwrap().to_string()).collect();
        assert_eq!(names, ["remember", "forget", READ_DOCS_TOOL]);
        assert!(tools.run("remember", &json!({"fact": "likes tea"})).await.is_ok());
    }
}
