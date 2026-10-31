use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use threatflux_anthropic_sdk::Client as Anthropic;

use crate::llm;
use crate::message_log::{LoggedMessage, PromptRecord, format_history_block, single_line};
use crate::throttle::Throttle;
use crate::usage::UsageTracker;

pub const MODEL: &str = "claude-haiku-5-5";
const TOOL_NAME: &str = "classify_message";
/// Label `classify_message`'s calls are recorded under in `UsageTracker`.
pub const USAGE_LABEL: &str = "classify";

const SYSTEM_PROMPT: &str = "You classify a single chat message from a Matrix room into a structured \
record. Always respond by calling the classify_message tool exactly once. You may be given the room's \
recent chat history for context; each line is a distinct message from exactly the sender in its own \
brackets. The message to classify is given separately afterward, marked `Message to classify, from \
[sender]`. Classify only that message, using the history for context (a short reply like \"yeah\" only \
makes sense in light of what preceded it). Base every field only on the text you are given; do not \
invent sender, room, or timestamp information.";

/// Structured analysis of a single free-text message, extracted by the LLM.
///
/// Metadata the bot already has from the Matrix event (sender, room, timestamp,
/// reply-to) is intentionally not part of this schema — it's attached by the
/// caller, not re-derived by the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageAnalysis {
    pub intent: Intent,
    pub confidence: f32,
    pub requires_response: bool,
    pub summary: String,
    pub sentiment: Sentiment,
    #[serde(default)]
    pub entities: Vec<Entity>,
    #[serde(default)]
    pub command: Option<CommandInfo>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Intent {
    Question,
    Command,
    Greeting,
    Farewell,
    Chitchat,
    Complaint,
    RequestHelp,
    Acknowledgement,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sentiment {
    Positive,
    Neutral,
    Negative,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entity {
    #[serde(rename = "type")]
    pub kind: EntityKind,
    pub value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityKind {
    Person,
    Date,
    Time,
    Url,
    Location,
    Topic,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandInfo {
    pub name: Option<String>,
    #[serde(default)]
    pub args: serde_json::Value,
}

/// Everything `classify_message` produces: the typed `analysis` every caller
/// actually needs, plus the exact `prompt` sent and the `raw_json` tool-call
/// input the model returned before it was parsed into `analysis` — recorded by
/// the caller (`handler::on_room_message`) so the status page can show both.
#[derive(Debug, Clone)]
pub struct ClassifyOutcome {
    pub analysis: MessageAnalysis,
    pub prompt: PromptRecord,
    pub raw_json: serde_json::Value,
}

impl MessageAnalysis {
    /// Stands in for classification that didn't run or failed, so the message still
    /// gets logged and, if it was addressed to the bot, answered.
    pub fn unclassified(intent: Intent, summary: &str) -> Self {
        Self {
            intent,
            confidence: 1.0,
            requires_response: false,
            summary: summary.to_string(),
            sentiment: Sentiment::Neutral,
            entities: Vec::new(),
            command: None,
        }
    }
}

/// Classifies a single message's free text into a `MessageAnalysis` by forcing
/// the model to call the `classify_message` tool with our schema as its input.
///
/// `history` (typically `MessageLogger::recent`, called before this message is
/// itself logged) gives short messages like "yeah" their context.
pub async fn classify_message(
    client: &Anthropic,
    message_text: &str,
    sender_name: &str,
    history: &[LoggedMessage],
    bot_name: &str,
    usage_tracker: &UsageTracker,
    rate_limit: &Throttle,
) -> Result<ClassifyOutcome> {
    if usage_tracker.over_cost_limit() {
        anyhow::bail!("Claude API cost limit reached; not classifying");
    }
    if let Some(remaining) = rate_limit.remaining() {
        anyhow::bail!("Claude API is rate-limited; not classifying for another {remaining:?}");
    }

    let mut user_turn = format_history_block(history, bot_name);
    user_turn.push_str(&format!("Message to classify, from [{sender_name}]:\n{}", single_line(message_text)));

    let body = request_body(&user_turn);
    let response = llm::send(client, body, None, USAGE_LABEL, usage_tracker, rate_limit)
        .await
        .context("classify_message request failed")?;

    let input = tool_input(&response).context("model response did not include a classify_message tool call")?;
    let analysis = serde_json::from_value(input.clone()).context("failed to parse classify_message tool input")?;

    Ok(ClassifyOutcome {
        analysis,
        prompt: PromptRecord { system: SYSTEM_PROMPT.to_string(), user: user_turn },
        raw_json: input,
    })
}

fn request_body(user_turn: &str) -> serde_json::Value {
    json!({
        "model": MODEL,
        "max_tokens": 1024,
        "system": SYSTEM_PROMPT,
        "messages": [{"role": "user", "content": user_turn}],
        "tools": [classify_tool()],
        "tool_choice": {"type": "tool", "name": TOOL_NAME},
    })
}

fn tool_input(response: &serde_json::Value) -> Option<serde_json::Value> {
    response["content"]
        .as_array()?
        .iter()
        .find(|block| block["type"] == "tool_use" && block["name"] == TOOL_NAME)
        .map(|block| block["input"].clone())
}

fn classify_tool() -> serde_json::Value {
    let input_schema = json!({
        "type": "object",
        "properties": {
            "intent": {
                "type": "string",
                "enum": [
                    "question", "command", "greeting", "farewell", "chitchat",
                    "complaint", "request_help", "acknowledgement", "other"
                ],
                "description": "The primary purpose of the message."
            },
            "confidence": {
                "type": "number",
                "minimum": 0,
                "maximum": 1,
                "description": "Confidence in the intent classification, from 0 to 1."
            },
            "requires_response": {
                "type": "boolean",
                "description": "Whether this is worth a reply from a bot in the room, as opposed to ambient chat between humans. A greeting to the room counts."
            },
            "summary": {
                "type": "string",
                "description": "A one-sentence paraphrase of what the message is about."
            },
            "sentiment": {
                "type": "string",
                "enum": ["positive", "neutral", "negative"]
            },
            "entities": {
                "type": "array",
                "description": "Named things mentioned in the message.",
                "items": {
                    "type": "object",
                    "properties": {
                        "type": {
                            "type": "string",
                            "enum": ["person", "date", "time", "url", "location", "topic", "other"]
                        },
                        "value": { "type": "string" }
                    },
                    "required": ["type", "value"]
                }
            }
        },
        "required": ["intent", "confidence", "requires_response", "summary", "sentiment", "entities"]
    });

    json!({
        "name": TOOL_NAME,
        "description": "Record a structured classification of the user's message.",
        "input_schema": input_schema,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_forces_the_classify_tool() {
        let body = request_body("hello");
        assert_eq!(body["tool_choice"], json!({"type": "tool", "name": TOOL_NAME}));
        assert_eq!(body["tools"][0]["name"], TOOL_NAME);
    }

    #[test]
    fn tool_input_is_found_after_a_leading_thinking_block() {
        let response = json!({"content": [
            {"type": "thinking", "thinking": "", "signature": "x"},
            {"type": "tool_use", "id": "1", "name": TOOL_NAME, "input": {"intent": "greeting"}},
        ]});
        assert_eq!(tool_input(&response), Some(json!({"intent": "greeting"})));
    }

    #[test]
    fn tool_input_is_none_without_a_tool_call() {
        assert_eq!(tool_input(&json!({"content": []})), None);
    }
}
