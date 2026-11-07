//! The one place that talks to the Messages API.
//!
//! Requests and responses are handled as raw JSON rather than through the SDK's
//! typed `MessageBuilder`/`MessageResponse`: `pause_turn` continuations must send
//! the assistant's content back byte-for-byte (including thinking and server-tool
//! blocks the SDK's types don't model), and newer request fields (`output_config`,
//! `fallbacks`) aren't in the builder.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine;
use serde_json::{Value, json};
use threatflux_anthropic_sdk::Client as Anthropic;
use threatflux_anthropic_sdk::models::Usage;
use threatflux_anthropic_sdk::types::{HttpMethod, RequestOptions};

use crate::throttle::{Throttle, is_rate_limited};
use crate::usage::UsageTracker;

/// Server-side refusal fallback. Only sent for models that accept it.
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";

/// A web-searching reply can legitimately take far longer than the SDK's 60s default.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(180);

/// How many times a turn is continued, for `pause_turn` or local tool calls, before
/// giving up and using what came back.
const MAX_CONTINUATIONS: usize = 6;

/// Shown when the model declines and no fallback rescued the request.
pub const REFUSAL_REPLY: &str = "Yeah, I'm not touching that one.";

/// Appended to a reply that ran out of tokens, so it doesn't just stop mid-sentence.
const TRUNCATED_SUFFIX: &str = " … (ran out of room there)";

/// An image sent alongside the user's text.
#[derive(Debug, Clone)]
pub struct ImageInput {
    pub media_type: &'static str,
    /// Base64-encoded file contents.
    pub data: String,
}

/// Claude's limit is 5MB of base64, which is this many bytes of file.
pub const MAX_IMAGE_BYTES: usize = 3_750_000;

/// Identifies an image by its first bytes rather than trusting the sender's or the
/// website's stated type.
fn image_media_type(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        [0x89, b'P', b'N', b'G', ..] => Some("image/png"),
        [0xFF, 0xD8, 0xFF, ..] => Some("image/jpeg"),
        [b'G', b'I', b'F', b'8', ..] => Some("image/gif"),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some("image/webp"),
        _ => None,
    }
}

impl ImageInput {
    /// Prepares a file for Claude, or `None` if it's too big or not an image Claude reads.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() > MAX_IMAGE_BYTES {
            return None;
        }
        Some(Self { media_type: image_media_type(bytes)?, data: base64::engine::general_purpose::STANDARD.encode(bytes) })
    }
}

/// Tools the bot runs itself, as opposed to server tools Anthropic runs.
pub trait LocalTools: Sync {
    fn definitions(&self) -> Vec<Value>;
    /// Runs a tool, returning its result or an error for the model to read.
    fn run<'a>(&'a self, name: &'a str, input: &'a Value) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>>;
}

#[derive(Default)]
pub struct CompletionRequest<'a> {
    pub model: &'a str,
    pub system: &'a str,
    pub user: &'a str,
    /// Caches the tools, system prompt, and `stable_context`. Only for calls whose
    /// system prompt doesn't change and that repeat often enough to pay back the
    /// write premium. A prefix under the model's minimum is silently not cached.
    pub cache: bool,
    /// Sent ahead of `user` and cached with the prefix, for context that stays the
    /// same from one request to the next.
    pub stable_context: &'a str,
    /// Thinking counts toward this on current models, so it must leave room for both.
    pub max_tokens: u32,
    /// `output_config.effort`; `None` keeps the model's default.
    pub effort: Option<&'a str>,
    /// Enables the web search tool, capped at this many searches per reply.
    pub web_search_max_uses: Option<u32>,
    /// Enables reading URLs from the conversation, capped at this many per reply.
    pub web_fetch_max_uses: Option<u32>,
    pub images: &'a [ImageInput],
    pub local_tools: Option<&'a dyn LocalTools>,
    /// Label the call is recorded under in `UsageTracker`.
    pub label: &'a str,
}

/// Sends a Messages API request, records its usage, and returns the raw response.
pub async fn send(
    client: &Anthropic,
    body: Value,
    beta: Option<&str>,
    label: &str,
    usage_tracker: &UsageTracker,
    rate_limit: &Throttle,
) -> Result<Value> {
    let mut headers = HashMap::new();
    if let Some(beta) = beta {
        headers.insert("anthropic-beta".to_string(), beta.to_string());
    }
    let options = RequestOptions { headers, timeout: Some(REQUEST_TIMEOUT), ..Default::default() };

    let response: Value = match client.request(HttpMethod::Post, "/messages", Some(body.clone()), Some(options)).await {
        Ok(response) => response,
        Err(err) => {
            if is_rate_limited(&err) {
                rate_limit.note_rate_limited();
            }
            usage_tracker.record_failure(&err.to_string());
            return Err(anyhow::Error::new(err).context("request to Claude failed"));
        }
    };

    // A fallback can answer on a different model than requested, so price what ran.
    let model = response["model"].as_str().or(body["model"].as_str()).unwrap_or_default();
    let usage: Usage = serde_json::from_value(response["usage"].clone()).context("failed to parse usage")?;
    usage_tracker.record(label, model, &usage);

    Ok(response)
}

/// Runs a plain text completion through to a finished, sendable reply: resumes
/// `pause_turn`, and turns refusals and token cutoffs into text instead of
/// silently posting nothing or half a sentence. Local tool calls are run and
/// answered here too.
pub async fn complete(
    client: &Anthropic,
    request: &CompletionRequest<'_>,
    usage_tracker: &UsageTracker,
    rate_limit: &Throttle,
) -> Result<String> {
    let mut body = build_body(request);
    let beta = supports_fallback(request.model).then_some(FALLBACK_BETA);

    for attempt in 0..=MAX_CONTINUATIONS {
        let response = send(client, body.clone(), beta, request.label, usage_tracker, rate_limit).await?;
        let content = response["content"].as_array().cloned().unwrap_or_default();
        let stop_reason = response["stop_reason"].as_str().unwrap_or_default();

        let tool_results = match (stop_reason, request.local_tools) {
            ("tool_use", Some(tools)) => Some(run_local_tools(&content, tools).await),
            _ => None,
        };

        if (stop_reason == "pause_turn" || tool_results.is_some()) && attempt < MAX_CONTINUATIONS {
            let messages = body["messages"].as_array_mut().context("messages is not an array")?;
            messages.push(json!({"role": "assistant", "content": content}));
            if let Some(results) = tool_results {
                messages.push(json!({"role": "user", "content": results}));
            }
            // Each continuation resends everything, so from here on the whole
            // conversation is worth caching. Not on the first request, where the
            // tail is unique to this message and would never be read back.
            if request.cache {
                body["cache_control"] = cache_marker();
            }
            continue;
        }

        return finish_reply(&content, stop_reason);
    }

    unreachable!("the loop's last attempt always returns")
}

/// One `tool_result` for every local tool call in `content`, sent back together.
async fn run_local_tools(content: &[Value], tools: &dyn LocalTools) -> Vec<Value> {
    let mut results = Vec::new();
    for block in content.iter().filter(|block| block["type"] == "tool_use") {
        let name = block["name"].as_str().unwrap_or_default();
        let (result, is_error) = match tools.run(name, &block["input"]).await {
            Ok(result) => (result, false),
            Err(error) => (error, true),
        };
        results.push(json!({"type": "tool_result", "tool_use_id": block["id"], "content": result, "is_error": is_error}));
    }
    results
}

fn cache_marker() -> Value {
    json!({"type": "ephemeral"})
}

fn user_content(request: &CompletionRequest<'_>) -> Value {
    if request.images.is_empty() && request.stable_context.is_empty() {
        return json!(request.user);
    }
    let mut blocks = Vec::new();
    if !request.stable_context.is_empty() {
        let mut block = json!({"type": "text", "text": request.stable_context});
        if request.cache {
            block["cache_control"] = cache_marker();
        }
        blocks.push(block);
    }
    blocks.extend(request.images.iter().map(|image| {
        json!({
            "type": "image",
            "source": {"type": "base64", "media_type": image.media_type, "data": image.data},
        })
    }));
    blocks.push(json!({"type": "text", "text": request.user}));
    json!(blocks)
}

fn tool_definitions(request: &CompletionRequest<'_>) -> Vec<Value> {
    // Haiku only takes the basic variants, not the dynamic-filtering ones.
    let basic = request.model.contains("haiku");
    let mut tools = Vec::new();
    if let Some(max_uses) = request.web_search_max_uses {
        let tool_type = if basic { "web_search_20250305" } else { "web_search_20260209" };
        tools.push(json!({"type": tool_type, "name": "web_search", "max_uses": max_uses}));
    }
    if let Some(max_uses) = request.web_fetch_max_uses {
        let tool_type = if basic { "web_fetch_20250910" } else { "web_fetch_20260209" };
        tools.push(json!({"type": tool_type, "name": "web_fetch", "max_uses": max_uses}));
    }
    if let Some(local) = request.local_tools {
        tools.extend(local.definitions());
    }
    tools
}

fn build_body(request: &CompletionRequest<'_>) -> Value {
    let mut body = json!({
        "model": request.model,
        "max_tokens": request.max_tokens,
        "system": request.system,
        "messages": [{"role": "user", "content": user_content(request)}],
    });
    if request.cache {
        // The prefix a reply can always read, whatever happens to the history after it.
        body["system"] = json!([{"type": "text", "text": request.system, "cache_control": cache_marker()}]);
    }
    if let Some(effort) = request.effort {
        body["output_config"] = json!({"effort": effort});
    }
    let tools = tool_definitions(request);
    if !tools.is_empty() {
        body["tools"] = json!(tools);
    }
    if supports_fallback(request.model) {
        body["fallbacks"] = json!("default");
    }
    body
}

fn supports_fallback(model: &str) -> bool {
    matches!(model, "claude-opus-5-5" | "claude-sonnet-5-5")
}

/// Turns a finished response's content into the text to post.
fn finish_reply(content: &[Value], stop_reason: &str) -> Result<String> {
    if stop_reason == "refusal" {
        return Ok(REFUSAL_REPLY.to_string());
    }

    let text = final_text(content);
    if stop_reason == "max_tokens" {
        anyhow::ensure!(!text.is_empty(), "response hit max_tokens before producing any text");
        return Ok(format!("{}{TRUNCATED_SUFFIX}", text.trim_end()));
    }
    anyhow::ensure!(!text.is_empty(), "response had no text (stop_reason: {stop_reason})");
    Ok(text)
}

/// The answer text: the text blocks after the last tool activity, so "let me look
/// that up" narration before a search isn't posted. Blocks are concatenated with
/// nothing between them because citations split one sentence across several
/// blocks. Reads by block type, since thinking blocks can come first.
fn final_text(content: &[Value]) -> String {
    let is_text = |block: &Value| block["type"] == "text";
    let is_thinking = |block: &Value| matches!(block["type"].as_str(), Some("thinking" | "redacted_thinking"));

    let start = content.iter().rposition(|block| !is_text(block) && !is_thinking(block)).map_or(0, |index| index + 1);

    content[start..]
        .iter()
        .filter(|block| is_text(block))
        .filter_map(|block| block["text"].as_str())
        .collect::<String>()
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value: &str) -> Value {
        json!({"type": "text", "text": value})
    }

    #[test]
    fn final_text_drops_narration_before_a_search() {
        let content = vec![
            text("let me check that"),
            json!({"type": "server_tool_use", "id": "1", "name": "web_search", "input": {}}),
            json!({"type": "web_search_tool_result", "tool_use_id": "1", "content": []}),
            text("the answer"),
        ];
        assert_eq!(final_text(&content), "the answer");
    }

    #[test]
    fn final_text_keeps_a_cited_sentence_whole() {
        let content = vec![text("It launched "), text("in 2024"), text(" in the US.")];
        assert_eq!(final_text(&content), "It launched in 2024 in the US.");
    }

    #[test]
    fn final_text_skips_leading_thinking_blocks() {
        let content = vec![json!({"type": "thinking", "thinking": "", "signature": "x"}), text("hello")];
        assert_eq!(final_text(&content), "hello");
    }

    #[test]
    fn finish_reply_flags_truncation_instead_of_stopping_mid_sentence() {
        let reply = finish_reply(&[text("the first half of ")], "max_tokens").unwrap();
        assert_eq!(reply, format!("the first half of{TRUNCATED_SUFFIX}"));
    }

    #[test]
    fn finish_reply_is_an_error_when_thinking_used_every_token() {
        let content = vec![json!({"type": "thinking", "thinking": "", "signature": "x"})];
        assert!(finish_reply(&content, "max_tokens").is_err());
    }

    #[test]
    fn finish_reply_answers_a_refusal_without_reading_content() {
        assert_eq!(finish_reply(&[], "refusal").unwrap(), REFUSAL_REPLY);
    }

    #[test]
    fn build_body_includes_search_cap_effort_and_fallback() {
        let body = build_body(&CompletionRequest {
            model: "claude-opus-5-5",
            system: "s",
            user: "u",
            max_tokens: 100,
            effort: Some("medium"),
            web_search_max_uses: Some(3),
            label: "chat",
            ..Default::default()
        });
        assert_eq!(body["tools"][0]["type"], "web_search_20260209");
        assert_eq!(body["tools"][0]["max_uses"], 3);
        assert_eq!(body["output_config"]["effort"], "medium");
        assert_eq!(body["fallbacks"], "default");
    }

    #[test]
    fn build_body_puts_images_before_the_text() {
        let images = [ImageInput { media_type: "image/png", data: "abc".to_string() }];
        let body = build_body(&CompletionRequest { user: "what is this", images: &images, ..Default::default() });

        let content = &body["messages"][0]["content"];
        assert_eq!(content[0]["source"]["data"], "abc");
        assert_eq!(content[1], json!({"type": "text", "text": "what is this"}));
    }

    #[test]
    fn image_media_type_goes_by_the_file_not_the_sender() {
        assert_eq!(image_media_type(&[0x89, b'P', b'N', b'G', 0x0D]), Some("image/png"));
        assert_eq!(image_media_type(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(image_media_type(b"GIF89a"), Some("image/gif"));
        assert_eq!(image_media_type(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(image_media_type(b"<svg></svg>"), None);
    }

    #[test]
    fn image_input_reads_the_file_not_its_label() {
        let png = [0x89, b'P', b'N', b'G', 1, 2, 3];
        assert_eq!(ImageInput::from_bytes(&png).unwrap().media_type, "image/png");
        assert!(ImageInput::from_bytes(b"<html>not an image</html>").is_none());
    }

    #[test]
    fn image_input_refuses_a_file_over_claudes_limit() {
        let mut big = vec![0; MAX_IMAGE_BYTES + 1];
        big[..3].copy_from_slice(&[0xFF, 0xD8, 0xFF]);
        assert!(ImageInput::from_bytes(&big).is_none());
    }

    #[test]
    fn build_body_marks_the_system_prompt_and_stable_context_for_caching() {
        let images = [ImageInput { media_type: "image/png", data: "abc".to_string() }];
        let body = build_body(&CompletionRequest {
            system: "s",
            stable_context: "old history",
            user: "new message",
            images: &images,
            cache: true,
            ..Default::default()
        });

        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        let content = &body["messages"][0]["content"];
        assert_eq!(content[0]["text"], "old history");
        assert_eq!(content[0]["cache_control"]["type"], "ephemeral");
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[2], json!({"type": "text", "text": "new message"}));
        assert!(body.get("cache_control").is_none(), "the first request's tail is unique to it");
    }

    #[test]
    fn build_body_leaves_uncached_calls_alone() {
        let body = build_body(&CompletionRequest { system: "s", user: "u", ..Default::default() });
        assert_eq!(body["system"], "s");
        assert_eq!(body["messages"][0]["content"], "u");
    }

    #[test]
    fn build_body_uses_basic_web_tools_on_haiku() {
        let body = build_body(&CompletionRequest {
            model: "claude-haiku-5-5",
            web_search_max_uses: Some(1),
            web_fetch_max_uses: Some(1),
            ..Default::default()
        });
        assert_eq!(body["tools"][0]["type"], "web_search_20250305");
        assert_eq!(body["tools"][1]["type"], "web_fetch_20250910");
    }

    #[test]
    fn build_body_omits_fallback_and_tools_for_cheap_plain_calls() {
        let body = build_body(&CompletionRequest {
            model: "claude-haiku-5-5",
            system: "s",
            user: "u",
            max_tokens: 100,
            label: "summary",
            ..Default::default()
        });
        assert!(body.get("fallbacks").is_none());
        assert!(body.get("tools").is_none());
        assert!(body.get("output_config").is_none());
    }

    mod against_a_fake_server {
        use std::sync::{Arc, Mutex};

        use axum::{Json, Router, extract::State, http::HeaderMap, routing::post};
        use threatflux_anthropic_sdk::Config;

        use super::*;

        type Seen = Arc<Mutex<Vec<(Option<String>, Value)>>>;

        /// Answers each request with the next canned response, recording what it got.
        async fn serve(responses: Vec<Value>) -> (Anthropic, Seen) {
            let seen: Seen = Arc::default();
            let state = (Arc::clone(&seen), Arc::new(responses));

            async fn handler(
                State((seen, responses)): State<(Seen, Arc<Vec<Value>>)>,
                headers: HeaderMap,
                Json(body): Json<Value>,
            ) -> Json<Value> {
                let beta = headers.get("anthropic-beta").map(|value| value.to_str().unwrap().to_string());
                let mut seen = seen.lock().unwrap();
                seen.push((beta, body));
                Json(responses[seen.len() - 1].clone())
            }

            let app = Router::new().route("/v1/messages", post(handler)).with_state(state);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

            let base_url = format!("http://127.0.0.1:{port}/").parse().unwrap();
            (Anthropic::new(Config::new("test-key").unwrap().with_base_url(base_url)), seen)
        }

        fn response(content: Value, stop_reason: &str, model: &str) -> Value {
            json!({
                "id": "msg_1",
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": content,
                "stop_reason": stop_reason,
                "stop_sequence": null,
                "usage": {"input_tokens": 1000, "output_tokens": 1000},
            })
        }

        fn tracker(_label: &str) -> UsageTracker {
            UsageTracker::open(Arc::new(crate::db::Database::open_in_memory()), None).unwrap()
        }

        fn request() -> CompletionRequest<'static> {
            CompletionRequest {
                model: "claude-opus-5-5",
                system: "system",
                user: "question",
                max_tokens: 8000,
                effort: Some("medium"),
                web_search_max_uses: Some(3),
                label: "chat",
                ..Default::default()
            }
        }

        struct EchoTool;

        impl LocalTools for EchoTool {
            fn definitions(&self) -> Vec<Value> {
                vec![json!({"name": "echo", "description": "d", "input_schema": {"type": "object"}})]
            }

            fn run<'a>(&'a self, _name: &'a str, input: &'a Value) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
                let result = input["text"].as_str().map(String::from).ok_or_else(|| "text is required".to_string());
                Box::pin(std::future::ready(result))
            }
        }

        #[tokio::test]
        async fn runs_local_tools_and_returns_every_result_together() {
            let calls = json!([
                {"type": "tool_use", "id": "a", "name": "echo", "input": {"text": "hi"}},
                {"type": "tool_use", "id": "b", "name": "echo", "input": {}},
            ]);
            let (client, seen) = serve(vec![
                response(calls.clone(), "tool_use", "claude-opus-5-5"),
                response(json!([{"type": "text", "text": "done"}]), "end_turn", "claude-opus-5-5"),
            ])
            .await;

            let request = CompletionRequest { local_tools: Some(&EchoTool), ..request() };
            let usage = tracker("tools");
            let reply = complete(&client, &request, &usage, &Throttle::new(Duration::from_secs(1))).await.unwrap();

            assert_eq!(reply, "done");
            let seen = seen.lock().unwrap();
            let names: Vec<_> = seen[0].1["tools"].as_array().unwrap().iter().map(|tool| tool["name"].clone()).collect();
            assert_eq!(names, vec![json!("web_search"), json!("echo")]);
            assert_eq!(seen[1].1["messages"][1], json!({"role": "assistant", "content": calls}));
            assert_eq!(
                seen[1].1["messages"][2]["content"],
                json!([
                    {"type": "tool_result", "tool_use_id": "a", "content": "hi", "is_error": false},
                    {"type": "tool_result", "tool_use_id": "b", "content": "text is required", "is_error": true},
                ])
            );
        }

        #[tokio::test]
        async fn resumes_a_paused_turn_sending_its_content_back_unchanged() {
            let paused = json!([
                {"type": "thinking", "thinking": "", "signature": "sig"},
                {"type": "server_tool_use", "id": "1", "name": "web_search", "input": {"query": "q"}},
            ]);
            let (client, seen) = serve(vec![
                response(paused.clone(), "pause_turn", "claude-opus-5-5"),
                response(json!([{"type": "text", "text": "the answer"}]), "end_turn", "claude-opus-5-5"),
            ])
            .await;
            let usage = tracker("pause");

            let reply = complete(&client, &request(), &usage, &Throttle::new(Duration::from_secs(1))).await.unwrap();

            assert_eq!(reply, "the answer");
            let seen = seen.lock().unwrap();
            assert_eq!(seen.len(), 2);
            assert_eq!(seen[1].1["messages"][1], json!({"role": "assistant", "content": paused}));
            assert_eq!(usage.snapshot().overall.request_count, 2);
        }

        #[tokio::test]
        async fn a_continuation_caches_the_whole_conversation() {
            let paused = response(json!([{"type": "text", "text": "working"}]), "pause_turn", "claude-opus-5-5");
            let done = response(json!([{"type": "text", "text": "done"}]), "end_turn", "claude-opus-5-5");
            let (client, seen) = serve(vec![paused, done]).await;

            let cached = CompletionRequest { cache: true, ..request() };
            complete(&client, &cached, &tracker("cache"), &Throttle::new(Duration::from_secs(1))).await.unwrap();

            let seen = seen.lock().unwrap();
            assert!(seen[0].1.get("cache_control").is_none());
            assert_eq!(seen[1].1["cache_control"]["type"], "ephemeral");
        }

        #[tokio::test]
        async fn sends_the_fallback_beta_only_for_models_that_take_it() {
            let done = response(json!([{"type": "text", "text": "ok"}]), "end_turn", "claude-haiku-5-5");
            let (client, seen) = serve(vec![done.clone(), done]).await;
            let usage = tracker("beta");
            let throttle = Throttle::new(Duration::from_secs(1));

            complete(&client, &request(), &usage, &throttle).await.unwrap();
            let cheap = CompletionRequest { model: "claude-haiku-5-5", ..request() };
            complete(&client, &cheap, &usage, &throttle).await.unwrap();

            let seen = seen.lock().unwrap();
            assert_eq!(seen[0].0.as_deref(), Some(FALLBACK_BETA));
            assert_eq!(seen[1].0, None);
        }

        #[tokio::test]
        async fn prices_the_model_that_actually_answered() {
            let (client, _seen) =
                serve(vec![response(json!([{"type": "text", "text": "ok"}]), "end_turn", "claude-haiku-5-5")]).await;
            let usage = tracker("price");

            complete(&client, &request(), &usage, &Throttle::new(Duration::from_secs(1))).await.unwrap();

            // 1,000 tokens each way at Haiku 5.5's $0.10/$0.50, not Opus 5.5's rates.
            let cost = usage.snapshot().overall.estimated_cost_usd;
            assert!((cost - 0.0006).abs() < 1e-9, "{cost}");
        }
    }
}
