//! The subset of the Anthropic Messages request that matters for conversion.
//! Unknown fields and block types are ignored rather than rejected.

use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, Deserialize, Default)]
pub struct MessagesRequest {
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub messages: Vec<Message>,
    #[serde(default)]
    pub system: Option<SystemPrompt>,
    #[serde(default)]
    pub max_tokens: Option<u64>,
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub top_p: Option<f64>,
    #[serde(default)]
    pub stop_sequences: Option<Vec<String>>,
    #[serde(default)]
    pub stream: Option<bool>,
    #[serde(default)]
    pub tools: Option<Vec<Tool>>,
    #[serde(default)]
    pub tool_choice: Option<ToolChoice>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum SystemPrompt {
    Text(String),
    Blocks(Vec<SystemBlock>),
}

#[derive(Debug, Clone, Deserialize)]
pub struct SystemBlock {
    #[serde(default)]
    pub text: String,
}

impl SystemPrompt {
    pub fn text(&self) -> String {
        match self {
            SystemPrompt::Text(s) => s.clone(),
            SystemPrompt::Blocks(blocks) => blocks
                .iter()
                .map(|b| b.text.as_str())
                .filter(|t| !t.is_empty())
                .collect::<Vec<_>>()
                .join("\n\n"),
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    /// Mid-conversation system message: Claude Code appends harness notices this way.
    System,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    pub role: Role,
    #[serde(default)]
    pub content: Content,
    /// `"next_user_message"` on a turn-scoped system message: it is dropped once a user turn follows.
    #[serde(default)]
    pub clear_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Content {
    Text(String),
    Blocks(Vec<Block>),
}

impl Default for Content {
    fn default() -> Self {
        Content::Blocks(Vec::new())
    }
}

impl Content {
    pub fn blocks(&self) -> Vec<Block> {
        match self {
            Content::Text(text) => vec![Block::Text { text: text.clone() }],
            Content::Blocks(blocks) => blocks.clone(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    Text {
        #[serde(default)]
        text: String,
    },
    Image {
        source: MediaSource,
    },
    Document {
        source: MediaSource,
        #[serde(default)]
        title: Option<String>,
    },
    ToolUse {
        id: String,
        name: String,
        #[serde(default)]
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        #[serde(default)]
        content: Option<ToolResultContent>,
        #[serde(default)]
        is_error: Option<bool>,
    },
    Thinking {
        #[serde(default)]
        thinking: String,
        #[serde(default)]
        signature: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MediaSource {
    Base64 {
        #[serde(default)]
        media_type: String,
        #[serde(default)]
        data: String,
    },
    Url {
        url: String,
    },
    Text {
        #[serde(default)]
        data: String,
    },
    Content {
        #[serde(default)]
        content: Value,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ToolResultContent {
    Text(String),
    Blocks(Vec<Block>),
}

#[derive(Debug, Clone, Deserialize)]
pub struct Tool {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub input_schema: Option<Value>,
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolChoice {
    Auto {
        #[serde(default)]
        disable_parallel_tool_use: Option<bool>,
    },
    Any {
        #[serde(default)]
        disable_parallel_tool_use: Option<bool>,
    },
    Tool {
        name: String,
        #[serde(default)]
        disable_parallel_tool_use: Option<bool>,
    },
    #[serde(rename = "none")]
    NoTools,
}

impl ToolChoice {
    pub fn disable_parallel(&self) -> bool {
        match self {
            ToolChoice::Auto { disable_parallel_tool_use }
            | ToolChoice::Any { disable_parallel_tool_use }
            | ToolChoice::Tool { disable_parallel_tool_use, .. } => disable_parallel_tool_use.unwrap_or(false),
            ToolChoice::NoTools => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tolerates_unknown_fields_and_blocks() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "claude-sonnet-5.5",
            "max_tokens": 100,
            "some_future_field": {"x": 1},
            "system": [{"type": "text", "text": "a", "cache_control": {"type": "ephemeral"}}, {"type": "text", "text": "b"}],
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": [
                    {"type": "server_tool_use", "id": "s", "name": "web_search", "input": {}},
                    {"type": "redacted_thinking", "data": "zzz"},
                    {"type": "text", "text": "ok", "citations": []}
                ]},
                {"role": "user", "content": [{"type": "image", "source": {"type": "file", "file_id": "f"}}]}
            ],
            "tool_choice": {"type": "auto"},
            "thinking": {"type": "adaptive"}
        }))
        .unwrap();
        assert_eq!(req.system.unwrap().text(), "a\n\nb");
        let blocks = req.messages[1].content.blocks();
        assert!(matches!(blocks[0], Block::Other));
        assert!(matches!(blocks[1], Block::Other));
        assert!(matches!(&blocks[2], Block::Text { text } if text == "ok"));
        assert!(matches!(req.messages[2].content.blocks()[0], Block::Image { source: MediaSource::Other }));
    }
}
