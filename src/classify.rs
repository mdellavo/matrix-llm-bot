use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use threatflux_anthropic_sdk::Client as Anthropic;
use threatflux_anthropic_sdk::builders::message_builder::MessageBuilder;
use threatflux_anthropic_sdk::models::{ContentBlock, Tool, ToolChoice};

use crate::message_log::{LoggedMessage, PromptRecord, format_history_block, single_line};
use crate::throttle::{Throttle, is_rate_limited};
use crate::usage::UsageTracker;

pub const MODEL: &str = "claude-haiku-4-5";
const TOOL_NAME: &str = "classify_message";
/// Label `classify_message`'s calls are recorded under in `UsageTracker`.
pub const USAGE_LABEL: &str = "classify";

const SYSTEM_PROMPT_TEMPLATE: &str = "You classify a single chat message from a Matrix room into a \
structured record. Always respond by calling the classify_message tool exactly once. You may be \
given the room's recent chat history for context, one line per message as `[sender] message` \
(oldest first) — each line is a distinct message from exactly the sender named in its own \
brackets; never attribute a line's content to any other sender. The message to classify is given \
separately afterward, marked `Message to classify, from [sender]`; classify only that message, \
using the history for context (e.g. a short reply like \"yeah\" or \"no\" only makes sense in \
light of what preceded it) — never classify a history line itself or confuse its sender with the \
message's actual sender. Base every field only on the message text you are given; do not invent \
sender, room, or timestamp information that isn't in the text.\n\n\
When intent is \"command\", set `command.name` to the exact command name or alias invoked, and \
`command.args` to an object keyed by the exact argument names listed below for that command — \
not a name you invent yourself. Only include an argument if its value actually appears in the \
message text; never invent one. Known commands:\n{command_reference}";

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

/// Classifies a single message's free text into a `MessageAnalysis` by forcing
/// the model to call the `classify_message` tool with our schema as its input.
///
/// `history` (typically `MessageLogger::recent`, called before this message is
/// itself logged) is rendered via `format_history_block` ahead of the message,
/// giving the classifier the same room context chat/greeting replies already
/// get — a short message like "yeah" or "gsc" is otherwise ambiguous without
/// knowing what it's replying to.
///
/// `command_reference` (`SkillRegistry::command_reference`) lists every loaded
/// command's name, aliases, and expected `args` keys, so that when the message
/// turns out to be a command, the model extracts `command.args` under the same
/// key names the target skill's `resolve_args` will later validate against —
/// without it, the model has no way to know a skill expects `name` rather than
/// `strain`/`query`/anything else it might otherwise guess.
pub async fn classify_message(
    client: &Anthropic,
    message_text: &str,
    sender: &str,
    history: &[LoggedMessage],
    command_reference: &str,
    usage_tracker: &UsageTracker,
    rate_limit: &Throttle,
) -> Result<ClassifyOutcome> {
    if usage_tracker.over_cost_limit() {
        anyhow::bail!("Claude API cost limit reached; not classifying");
    }
    if let Some(remaining) = rate_limit.remaining() {
        anyhow::bail!("Claude API is rate-limited; not classifying for another {remaining:?}");
    }

    let tool = classify_tool()?;
    let system_prompt = SYSTEM_PROMPT_TEMPLATE.replace("{command_reference}", command_reference);

    let mut user_turn = format_history_block(history);
    user_turn.push_str(&format!("Message to classify, from [{sender}]:\n{}", single_line(message_text)));

    let params = MessageBuilder::new()
        .model(MODEL)
        .max_tokens(1024)
        .system(system_prompt.clone())
        .user(user_turn.clone())
        .tools(vec![tool])
        .tool_choice(ToolChoice::Tool {
            name: TOOL_NAME.to_string(),
        })
        .build();

    let message = match client.messages().create(params, None).await {
        Ok(message) => message,
        Err(err) => {
            if is_rate_limited(&err) {
                rate_limit.note_rate_limited();
            }
            return Err(anyhow::Error::new(err).context("classify_message request to Claude failed"));
        }
    };

    usage_tracker.record(USAGE_LABEL, MODEL, &message.usage);

    for block in message.content {
        if let ContentBlock::ToolUse { name, input, .. } = block
            && name == TOOL_NAME
        {
            let analysis = serde_json::from_value(input.clone()).context("failed to parse classify_message tool input")?;
            return Ok(ClassifyOutcome {
                analysis,
                prompt: PromptRecord { system: system_prompt, user: user_turn },
                raw_json: input,
            });
        }
    }

    anyhow::bail!("model response did not include a classify_message tool call")
}

fn classify_tool() -> Result<Tool> {
    let schema_json = json!({
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
                "description": "Whether the bot should reply to this message at all, as opposed to ambient chat between humans."
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
            },
            "command": {
                "type": "object",
                "description": "Only populate when intent is \"command\": the command name and its arguments.",
                "properties": {
                    "name": { "type": ["string", "null"] },
                    "args": { "type": "object" }
                }
            }
        },
        "required": ["intent", "confidence", "requires_response", "summary", "sentiment", "entities"]
    });

    Ok(Tool::new(
        TOOL_NAME,
        "Record a structured classification of the user's message.",
        schema_json,
    ))
}
