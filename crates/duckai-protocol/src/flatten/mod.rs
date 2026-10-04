//! 三协议输入扁平化（§1.3）。把 OpenAI / Anthropic / Responses 三种请求体归一为
//! `Vec<ChatTurn>` + `tools` + `tool_choice`，上游只认这一种形状。
//!
//! 保序策略沿用 Python 参考实现：
//! - 角色标签 `Human / Assistant / System`（[`flatten_conversation`] 单串出口）；
//! - tool 轮打 `[tool_result <id>]` 前缀，空轮跳过；
//! - system 文本在单串出口中裸置于最前。
//!
//! 回归焦点：P0-1（`_responses_input_text` NameError 死代码）——Responses 的四种
//! input 形态必须真实可解析（见 `responses.rs` 测试）。

pub mod anthropic;
pub mod openai;
pub mod responses;

pub use anthropic::flatten_anthropic;
pub use openai::flatten_openai;
pub use responses::flatten_responses;

use duckai_types::{ChatTurn, Role, ToolChoice, ToolDef};
use serde_json::Value;

/// 扁平化结果：三协议公共出口。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Flattened {
    pub turns: Vec<ChatTurn>,
    pub tools: Vec<ToolDef>,
    pub tool_choice: Option<ToolChoice>,
}

impl Flattened {
    pub fn new() -> Self {
        Self::default()
    }
}

/// 输入形态错误（API 层映射 400 InvalidInput）。
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("invalid input: {0}")]
pub struct FlattenError(pub String);

pub(crate) fn err(msg: impl Into<String>) -> FlattenError {
    FlattenError(msg.into())
}

/// 角色标签（Python `_role_label` 对齐）。
pub fn role_label(role: &Role) -> &'static str {
    match role {
        Role::User => "Human",
        Role::Assistant => "Assistant",
        Role::System => "System",
        Role::Tool => "Human",
    }
}

/// Python `flatten_conversation` 单串出口（工具信封/降级路径与回归测试用）：
/// system 裸文本 → 空行 → 每轮 `标签: 文本`，空轮跳过。
pub fn flatten_conversation(system: &str, messages: &[ChatTurn]) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !system.trim().is_empty() {
        parts.push(system.trim().to_string());
    }
    for m in messages {
        let text = m.content.text();
        if text.trim().is_empty() && m.role != Role::Tool {
            continue;
        }
        let label = if m.role == Role::Tool {
            let id = m.tool_call_id.clone().unwrap_or_default();
            format!("[tool_result {id}]")
        } else {
            role_label(&m.role).to_string()
        };
        parts.push(format!("{label}: {text}"));
    }
    parts.join("\n\n").trim().to_string()
}

/// JSON 文本 → `serde_json::Value`（字符串参数解析；裸文本包成字符串）。
pub(crate) fn parse_args(raw: &str) -> Value {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return serde_json::json!({});
    }
    serde_json::from_str(trimmed).unwrap_or_else(|_| Value::String(raw.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_labels_match_python() {
        assert_eq!(role_label(&Role::User), "Human");
        assert_eq!(role_label(&Role::Assistant), "Assistant");
        assert_eq!(role_label(&Role::System), "System");
    }

    #[test]
    fn flatten_conversation_parity() {
        let messages = vec![
            ChatTurn::text(Role::User, "hi"),
            ChatTurn::text(Role::Assistant, "hello"),
            ChatTurn::text(Role::System, "   "),
            ChatTurn::text(Role::User, ""),
        ];
        assert_eq!(
            flatten_conversation("be nice", &messages),
            "be nice\n\nHuman: hi\n\nAssistant: hello"
        );
    }

    #[test]
    fn flatten_conversation_tool_label() {
        let mut t = ChatTurn::text(Role::Tool, "out");
        t.tool_call_id = Some("call_abc".into());
        assert_eq!(
            flatten_conversation("", &[t]),
            "[tool_result call_abc]: out"
        );
    }
}
