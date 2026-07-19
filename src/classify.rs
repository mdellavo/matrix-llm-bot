use anthropic_sdk::types::{ContentBlock, MessageCreateBuilder, Tool, ToolChoice, ToolInputSchema};
use anthropic_sdk::Anthropic;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;

pub const MODEL: &str = "claude-haiku-4-5";
const TOOL_NAME: &str = "classify_message";

const SYSTEM_PROMPT: &str = "You classify a single chat message from a Matrix room into a \
structured record. Always respond by calling the classify_message tool exactly once. Base \
every field only on the message text you are given; do not invent sender, room, or timestamp \
information that isn't in the text.";

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

/// Classifies a single message's free text into a `MessageAnalysis` by forcing
/// the model to call the `classify_message` tool with our schema as its input.
pub async fn classify_message(client: &Anthropic, message_text: &str) -> Result<MessageAnalysis> {
    let tool = classify_tool()?;

    let params = MessageCreateBuilder::new(MODEL, 1024)
        .system(SYSTEM_PROMPT)
        .user(message_text)
        .tools(vec![tool])
        .tool_choice(ToolChoice::Tool {
            name: TOOL_NAME.to_string(),
        })
        .build();

    let message = client
        .messages()
        .create(params)
        .await
        .context("classify_message request to Claude failed")?;

    for block in message.content {
        if let ContentBlock::ToolUse { name, input, .. } = block
            && name == TOOL_NAME
        {
            return serde_json::from_value(input)
                .context("failed to parse classify_message tool input");
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

    let input_schema: ToolInputSchema =
        serde_json::from_value(schema_json).context("invalid classify_message tool schema")?;

    Ok(Tool {
        name: TOOL_NAME.to_string(),
        description: "Record a structured classification of the user's message.".to_string(),
        input_schema,
    })
}
