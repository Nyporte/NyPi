//! Provider-neutral message model. Field names serialize in Pi's camelCase JSON shape so
//! session files stay compatible with Pi's v3 format.

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

/// One block of message content.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum ContentBlock {
    Text {
        text: String,
        /// Provider replay data for this text (OpenAI Responses message id and phase, Gemini thought signature).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text_signature: Option<String>,
    },
    Thinking {
        thinking: String,
        /// Provider-specific opaque replay data (Anthropic signature, OpenAI reasoning field name).
        #[serde(rename = "thinkingSignature", default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        redacted: bool,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: Value,
        /// Set when the model's arguments text was not a JSON object; `arguments` is then `{}`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        arguments_error: Option<String>,
        /// Gemini thought signature, echoed back with the call.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thought_signature: Option<String>,
    },
    Image {
        /// Base64-encoded bytes.
        data: String,
        mime_type: String,
    },
}

impl ContentBlock {
    pub fn text(text: impl Into<String>) -> Self {
        ContentBlock::Text { text: text.into(), text_signature: None }
    }
}

/// Why an assistant response ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StopReason {
    Stop,
    Length,
    ToolUse,
    Error,
    Aborted,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub total: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub total_tokens: u64,
    #[serde(default)]
    pub cost: Cost,
}

impl Usage {
    /// Tokens the request occupied in the context window (input side plus generated output).
    pub fn context_tokens(&self) -> u64 {
        if self.total_tokens > 0 {
            self.total_tokens
        } else {
            self.input + self.output + self.cache_read + self.cache_write
        }
    }

    pub fn add(&mut self, other: &Usage) {
        self.input += other.input;
        self.output += other.output;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
        self.total_tokens += other.total_tokens;
        self.cost.input += other.cost.input;
        self.cost.output += other.cost.output;
        self.cost.cache_read += other.cost.cache_read;
        self.cost.cache_write += other.cost.cache_write;
        self.cost.total += other.cost.total;
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserMessage {
    #[serde(deserialize_with = "content_from_string_or_blocks")]
    pub content: Vec<ContentBlock>,
    pub timestamp: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessage {
    pub content: Vec<ContentBlock>,
    pub api: String,
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub usage: Usage,
    pub stop_reason: StopReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    pub timestamp: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultMessage {
    pub tool_call_id: String,
    pub tool_name: String,
    #[serde(deserialize_with = "content_from_string_or_blocks")]
    pub content: Vec<ContentBlock>,
    /// Structured details for logs and UI. Never sent to the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    pub is_error: bool,
    pub timestamp: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

/// A transcript message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "camelCase")]
pub enum Message {
    /// The system prompt and tools at a point in the transcript (Pi's transcript-carried
    /// system messages; nypi-ai/src/transcript.rs).
    System(SystemMessage),
    User(UserMessage),
    Assistant(AssistantMessage),
    ToolResult(ToolResultMessage),
}

/// System instructions and tool declarations at one point in the transcript. The leading one is
/// the system prompt; later ones change it: `content` adds instructions, `sections` replace
/// named sections (`None` removes one), and `tools_added`/`tools_removed` change the tool set.
/// Replaying every one in order yields the current prompt and tools.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemMessage {
    #[serde(default, deserialize_with = "text_from_string_or_blocks")]
    pub content: String,
    #[serde(default, skip_serializing_if = "Sections::is_empty")]
    pub sections: Sections,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools_added: Vec<ToolDefinition>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools_removed: Vec<ToolReference>,
    pub timestamp: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolReference {
    pub name: String,
}

/// Named prompt sections in order, serialized as a JSON object (Pi's `Record`), `null` for a
/// removed one. Order matters: the prompt renders them in it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Sections(pub Vec<(String, Option<String>)>);

impl Sections {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn set(&mut self, name: &str, value: Option<String>) {
        match self.0.iter_mut().find(|(n, _)| n == name) {
            Some((_, v)) => *v = value,
            None => self.0.push((name.to_string(), value)),
        }
    }
}

impl Serialize for Sections {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (name, value) in &self.0 {
            map.serialize_entry(name, value)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for Sections {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Sections;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an object of prompt sections")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Sections, A::Error> {
                let mut sections = Vec::new();
                while let Some((name, value)) = map.next_entry::<String, Option<String>>()? {
                    sections.push((name, value));
                }
                Ok(Sections(sections))
            }
        }
        deserializer.deserialize_map(Visitor)
    }
}

impl Message {
    pub fn user_text(text: impl Into<String>) -> Self {
        Message::User(UserMessage { content: vec![ContentBlock::text(text)], timestamp: now_ms() })
    }

    pub fn role(&self) -> &'static str {
        match self {
            Message::System(_) => "system",
            Message::User(_) => "user",
            Message::Assistant(_) => "assistant",
            Message::ToolResult(_) => "toolResult",
        }
    }
}

impl AssistantMessage {
    /// Empty assistant message for `model`, used as the base of a streamed response.
    pub fn empty(api: &str, provider: &str, model: &str) -> Self {
        AssistantMessage {
            content: Vec::new(),
            api: api.to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: now_ms(),
            duration_ms: None,
        }
    }

    /// Concatenated text blocks.
    pub fn text(&self) -> String {
        text_of(&self.content)
    }

    /// Tool-call blocks in source order.
    pub fn tool_calls(&self) -> Vec<ToolCall> {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolCall { id, name, arguments, arguments_error, .. } => Some(ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    arguments: arguments.clone(),
                    arguments_error: arguments_error.clone(),
                }),
                _ => None,
            })
            .collect()
    }
}

/// An owned view of a tool-call block.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
    #[serde(default)]
    pub arguments_error: Option<String>,
}

/// Tool declaration sent to the model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    /// JSON Schema of the arguments object.
    pub parameters: Value,
}

/// Everything a provider needs for one request.
#[derive(Clone, Debug, Default)]
pub struct Context {
    pub system_prompt: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
}

/// Concatenate the text blocks of `content`.
pub fn text_of(content: &[ContentBlock]) -> String {
    let mut out = String::new();
    for block in content {
        if let ContentBlock::Text { text, .. } = block {
            out.push_str(text);
        }
    }
    out
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

// Pi stores user and tool-result content either as a plain string or as a block list.
fn text_from_string_or_blocks<'de, D: Deserializer<'de>>(de: D) -> Result<String, D::Error> {
    Ok(text_of(&content_from_string_or_blocks(de)?))
}

/// Content as Pi stores it: a plain string or a block list (for `deserialize_with`).
pub fn content_from_string_or_blocks<'de, D: Deserializer<'de>>(de: D) -> Result<Vec<ContentBlock>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Either {
        Text(String),
        Blocks(Vec<ContentBlock>),
    }
    Ok(match Either::deserialize(de)? {
        Either::Text(text) => vec![ContentBlock::Text { text, text_signature: None }],
        Either::Blocks(blocks) => blocks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_round_trips_in_pi_shape() {
        let json = r#"{"role":"assistant","content":[{"type":"thinking","thinking":"hm","thinkingSignature":"sig"},{"type":"toolCall","id":"t1","name":"read","arguments":{"path":"a"}}],"api":"anthropic-messages","provider":"anthropic","model":"m","usage":{"input":1,"output":2,"cacheRead":0,"cacheWrite":0,"totalTokens":3,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"stopReason":"toolUse","timestamp":5}"#;
        let message: Message = serde_json::from_str(json).unwrap();
        let Message::Assistant(assistant) = &message else { panic!("expected assistant") };
        assert_eq!(assistant.stop_reason, StopReason::ToolUse);
        assert_eq!(assistant.tool_calls()[0].name, "read");
        let back = serde_json::to_value(&message).unwrap();
        assert_eq!(back["content"][0]["thinkingSignature"], "sig");
        assert_eq!(back["content"][1]["type"], "toolCall");
    }

    #[test]
    fn user_content_accepts_plain_string() {
        let message: Message = serde_json::from_str(r#"{"role":"user","content":"hi","timestamp":1}"#).unwrap();
        let Message::User(user) = message else { panic!("expected user") };
        assert_eq!(text_of(&user.content), "hi");
    }
}
