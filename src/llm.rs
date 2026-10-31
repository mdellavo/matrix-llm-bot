//! The one place that talks to the Messages API.
//!
//! Requests and responses are handled as raw JSON rather than through the SDK's
//! typed `MessageBuilder`/`MessageResponse`: `pause_turn` continuations must send
//! the assistant's content back byte-for-byte (including thinking and server-tool
//! blocks the SDK's types don't model), and newer request fields (`output_config`,
//! `fallbacks`) aren't in the builder.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result};
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

/// How many times a `pause_turn` is resumed before giving up and using what came back.
const MAX_CONTINUATIONS: usize = 3;

/// Shown when the model declines and no fallback rescued the request.
pub const REFUSAL_REPLY: &str = "Yeah, I'm not touching that one.";

/// Appended to a reply that ran out of tokens, so it doesn't just stop mid-sentence.
const TRUNCATED_SUFFIX: &str = " … (ran out of room there)";

pub struct CompletionRequest<'a> {
    pub model: &'a str,
    pub system: &'a str,
    pub user: &'a str,
    /// Thinking counts toward this on current models, so it must leave room for both.
    pub max_tokens: u32,
    /// `output_config.effort`; `None` keeps the model's default.
    pub effort: Option<&'a str>,
    /// Enables the web search tool, capped at this many searches per reply.
    pub web_search_max_uses: Option<u32>,
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
/// silently posting nothing or half a sentence.
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

        if stop_reason == "pause_turn" && attempt < MAX_CONTINUATIONS {
            body["messages"].as_array_mut().context("messages is not an array")?.push(json!({
                "role": "assistant",
                "content": content,
            }));
            continue;
        }

        return finish_reply(&content, stop_reason);
    }

    unreachable!("the loop's last attempt always returns")
}

fn build_body(request: &CompletionRequest<'_>) -> Value {
    let mut body = json!({
        "model": request.model,
        "max_tokens": request.max_tokens,
        "system": request.system,
        "messages": [{"role": "user", "content": request.user}],
    });
    if let Some(effort) = request.effort {
        body["output_config"] = json!({"effort": effort});
    }
    if let Some(max_uses) = request.web_search_max_uses {
        body["tools"] = json!([{"type": "web_search_20260209", "name": "web_search", "max_uses": max_uses}]);
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
        });
        assert_eq!(body["tools"][0]["type"], "web_search_20260209");
        assert_eq!(body["tools"][0]["max_uses"], 3);
        assert_eq!(body["output_config"]["effort"], "medium");
        assert_eq!(body["fallbacks"], "default");
    }

    #[test]
    fn build_body_omits_fallback_and_tools_for_cheap_plain_calls() {
        let body = build_body(&CompletionRequest {
            model: "claude-haiku-5-5",
            system: "s",
            user: "u",
            max_tokens: 100,
            effort: None,
            web_search_max_uses: None,
            label: "summary",
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

        fn tracker(label: &str) -> UsageTracker {
            let path = std::env::temp_dir().join(format!("llm-test-{label}-{}.json", std::process::id()));
            let _ = std::fs::remove_file(&path);
            UsageTracker::open(&path, None).unwrap()
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
            }
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
