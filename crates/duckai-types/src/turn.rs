use serde::{Deserialize, Serialize};

/// 对话角色（协议无关；三协议在 flatten 时归一化到这里）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }
}

/// 内容块（tool_use / tool_result / 纯文本）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
    Image {
        media_type: String,
        /// base64 数据；纯 HTTP 上游不支持图片时由 flatten 转为占位文本。
        data: String,
    },
}

/// 轮次内容：纯文本或块列表。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TurnContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

impl TurnContent {
    pub fn text(&self) -> String {
        match self {
            TurnContent::Text(s) => s.clone(),
            TurnContent::Blocks(blocks) => blocks
                .iter()
                .map(|b| match b {
                    ContentBlock::Text { text } => text.clone(),
                    ContentBlock::ToolUse { name, input, .. } => {
                        format!("[tool_use {name}]: {input}")
                    }
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } => format!("[tool_result {tool_use_id}]: {content}"),
                    ContentBlock::Image { .. } => "[image omitted]".to_string(),
                })
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.text().trim().is_empty()
    }
}

/// 一次对话轮次。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatTurn {
    pub role: Role,
    pub content: TurnContent,
    /// 多轮上下文中的角色标签（flatten_conversation 保序策略，§1.3）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ChatTurn {
    pub fn text(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: TurnContent::Text(content.into()),
            name: None,
            tool_call_id: None,
        }
    }
}

/// 工具定义（OpenAI `parameters` 与 Anthropic `input_schema` 归一）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema（OpenAI: parameters；Anthropic: input_schema）。
    #[serde(default = "empty_object")]
    pub input_schema: serde_json::Value,
}

fn empty_object() -> serde_json::Value {
    serde_json::json!({})
}

/// 工具选择策略。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolChoice {
    Auto,
    None,
    Required,
    Named(String),
}
