//! Anthropic Messages API 输入扁平化。
//!
//! 覆盖：`system`（字符串或文本块）、user/assistant 消息（text / image / tool_use /
//! tool_result 块）、`tools`（`input_schema`）、`tool_choice`（auto/none/any/tool）。

use serde::Deserialize;
use serde_json::Value;

use duckai_types::{ChatTurn, ContentBlock, Role, ToolChoice, ToolDef, TurnContent};

use super::{FlattenError, Flattened, err};

#[derive(Debug, Deserialize)]
pub struct AnthropicInput {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub system: Option<Value>,
    #[serde(default)]
    pub messages: Vec<AnthropicMessage>,
    #[serde(default)]
    pub tools: Vec<AnthropicTool>,
    #[serde(default)]
    pub tool_choice: Option<AnthropicToolChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct AnthropicMessage {
    pub role: String,
    pub content: Value,
}

#[derive(Debug, Deserialize)]
pub struct AnthropicTool {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub input_schema: Option<Value>,
}

#[derive(Debug, Deserialize)]
pub struct AnthropicToolChoice {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub name: Option<String>,
}

/// 扁平化一次 `/v1/messages` 输入。
pub fn flatten_anthropic(input: &AnthropicInput) -> Result<Flattened, FlattenError> {
    if input.messages.is_empty() {
        return Err(err("messages must not be empty"));
    }
    let mut out = Flattened::new();

    if let Some(system) = &input.system {
        let text = match system {
            Value::String(s) => s.clone(),
            Value::Array(blocks) => blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(Value::as_str).map(str::to_string))
                .collect::<Vec<_>>()
                .join("\n"),
            other => other.to_string(),
        };
        if !text.trim().is_empty() {
            out.turns.push(ChatTurn::text(Role::System, text));
        }
    }

    for msg in &input.messages {
        let role = match msg.role.as_str() {
            "user" => Role::User,
            "assistant" => Role::Assistant,
            other => return Err(err(format!("unknown message role: {other}"))),
        };
        match &msg.content {
            Value::String(s) => {
                if !s.trim().is_empty() {
                    out.turns.push(ChatTurn::text(role, s.clone()));
                }
            }
            Value::Array(blocks) => {
                if role == Role::Assistant {
                    // 助手消息的多个块合并为**一条**轮次（text/thinking + tool_use 同轮），
                    // 与 openai.rs 的 content+tool_calls 合并语义对齐
                    let mut acc: Vec<ContentBlock> = Vec::new();
                    for block in blocks {
                        collect_assistant_block(&mut acc, block)?;
                    }
                    if !acc.is_empty() {
                        out.turns.push(ChatTurn {
                            role,
                            content: TurnContent::Blocks(acc),
                            name: None,
                            tool_call_id: None,
                        });
                    }
                } else {
                    for block in blocks {
                        push_anthropic_block(&mut out, role, block)?;
                    }
                }
            }
            _ => return Err(err("message content must be string or array")),
        }
    }

    if out.turns.is_empty() {
        return Err(err("messages produced no content"));
    }

    out.tools = input
        .tools
        .iter()
        .map(|t| ToolDef {
            name: t.name.clone(),
            description: t.description.clone(),
            input_schema: t
                .input_schema
                .clone()
                .unwrap_or_else(|| serde_json::json!({})),
        })
        .collect();
    out.tool_choice = input
        .tool_choice
        .as_ref()
        .map(tool_choice_anthropic)
        .transpose()?;
    Ok(out)
}

fn push_anthropic_block(
    out: &mut Flattened,
    role: Role,
    block: &Value,
) -> Result<(), FlattenError> {
    let kind = block
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| err("content block missing type"))?;
    match kind {
        "text" => {
            let text = block
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !text.trim().is_empty() {
                out.turns.push(ChatTurn {
                    role,
                    content: TurnContent::Text(text.to_string()),
                    name: None,
                    tool_call_id: None,
                });
            }
        }
        "thinking" => {
            let text = block
                .get("thinking")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !text.trim().is_empty() {
                out.turns.push(ChatTurn::text(role, text));
            }
        }
        "image" => {
            let source = block.get("source").cloned().unwrap_or(Value::Null);
            let media_type = source
                .get("media_type")
                .and_then(Value::as_str)
                .unwrap_or("image/png")
                .to_string();
            let data = source
                .get("data")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            out.turns.push(ChatTurn {
                role,
                content: TurnContent::Blocks(vec![ContentBlock::Image { media_type, data }]),
                name: None,
                tool_call_id: None,
            });
        }
        "tool_use" => {
            let id = block
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let name = block
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let input = block.get("input").cloned().unwrap_or(Value::Null);
            out.turns.push(ChatTurn {
                role: Role::Assistant,
                content: TurnContent::Blocks(vec![ContentBlock::ToolUse { id, name, input }]),
                name: None,
                tool_call_id: None,
            });
        }
        "tool_result" => {
            let tool_use_id = block
                .get("tool_use_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let is_error = block
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let content = match block.get("content") {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Array(items)) => items
                    .iter()
                    .filter_map(|b| b.get("text").and_then(Value::as_str).map(str::to_string))
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => String::new(),
            };
            out.turns.push(ChatTurn {
                role: Role::Tool,
                content: TurnContent::Text(content),
                name: None,
                tool_call_id: Some(tool_use_id),
            });
            let _ = is_error;
        }
        other => return Err(err(format!("unknown content block type: {other}"))),
    }
    Ok(())
}

/// 助手消息块收集：合并进单一轮次（Text/Thinking→Text，tool_use→ToolUse，image→Image）。
fn collect_assistant_block(acc: &mut Vec<ContentBlock>, block: &Value) -> Result<(), FlattenError> {
    let kind = block
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| err("content block missing type"))?;
    match kind {
        "text" | "thinking" => {
            let field = if kind == "text" { "text" } else { "thinking" };
            let text = block.get(field).and_then(Value::as_str).unwrap_or_default();
            if !text.trim().is_empty() {
                acc.push(ContentBlock::Text {
                    text: text.to_string(),
                });
            }
        }
        "image" => {
            let source = block.get("source").cloned().unwrap_or(Value::Null);
            let media_type = source
                .get("media_type")
                .and_then(Value::as_str)
                .unwrap_or("image/png")
                .to_string();
            let data = source
                .get("data")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            acc.push(ContentBlock::Image { media_type, data });
        }
        "tool_use" => {
            let id = block
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let name = block
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let input = block.get("input").cloned().unwrap_or(Value::Null);
            acc.push(ContentBlock::ToolUse { id, name, input });
        }
        other => return Err(err(format!("unknown content block type: {other}"))),
    }
    Ok(())
}

fn tool_choice_anthropic(v: &AnthropicToolChoice) -> Result<ToolChoice, FlattenError> {
    match v.kind.as_str() {
        "auto" => Ok(ToolChoice::Auto),
        "none" => Ok(ToolChoice::None),
        "any" => Ok(ToolChoice::Required),
        "tool" => v
            .name
            .clone()
            .map(ToolChoice::Named)
            .ok_or_else(|| err("tool_choice.type=tool requires name")),
        other => Err(err(format!("unknown tool_choice type: {other}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn full_input() -> AnthropicInput {
        serde_json::from_value(json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 1024,
            "system": "be brief",
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "read /tmp/x"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "QUJD"}}
                ]},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "reading"},
                    {"type": "tool_use", "id": "tu_1", "name": "Read", "input": {"file_path": "/tmp/x"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "tu_1", "content": "file body", "is_error": false}
                ]}
            ],
            "tools": [{"name": "Read", "description": "read", "input_schema": {"type": "object"}}],
            "tool_choice": {"type": "auto"}
        }))
        .unwrap()
    }

    #[test]
    fn system_and_block_roles() {
        let flat = flatten_anthropic(&full_input()).unwrap();
        assert_eq!(
            flat.turns.iter().map(|t| t.role).collect::<Vec<_>>(),
            vec![
                Role::System,
                Role::User,
                Role::User,
                Role::Assistant,
                Role::Tool
            ]
        );
        assert_eq!(flat.turns[0].content.text(), "be brief");
        match &flat.turns[2].content {
            TurnContent::Blocks(blocks) => assert!(
                matches!(&blocks[0], ContentBlock::Image { media_type, .. } if media_type == "image/jpeg")
            ),
            other => panic!("{other:?}"),
        }
        match &flat.turns[3].content {
            TurnContent::Blocks(blocks) => {
                assert!(matches!(&blocks[1], ContentBlock::ToolUse { id, .. } if id == "tu_1"))
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(flat.turns[4].tool_call_id.as_deref(), Some("tu_1"));
        assert_eq!(flat.turns[4].content.text(), "file body");
        assert_eq!(flat.tools[0].name, "Read");
        assert!(matches!(flat.tool_choice, Some(ToolChoice::Auto)));
    }

    #[test]
    fn system_as_blocks_and_string_roles() {
        let input: AnthropicInput = serde_json::from_value(json!({
            "model": "m",
            "system": [{"type": "text", "text": "s1"}, {"type": "text", "text": "s2"}],
            "messages": [
                {"role": "user", "content": "hello"},
                {"role": "assistant", "content": "world"}
            ]
        }))
        .unwrap();
        let flat = flatten_anthropic(&input).unwrap();
        assert_eq!(flat.turns[0].content.text(), "s1\ns2");
        assert_eq!(flat.turns[1].content.text(), "hello");
        assert_eq!(flat.turns[2].content.text(), "world");
    }

    #[test]
    fn tool_choice_any_and_tool() {
        let input: AnthropicInput = serde_json::from_value(json!({
            "model": "m",
            "messages": [{"role": "user", "content": "x"}],
            "tool_choice": {"type": "any"}
        }))
        .unwrap();
        assert!(matches!(
            flatten_anthropic(&input).unwrap().tool_choice,
            Some(ToolChoice::Required)
        ));
        let input: AnthropicInput = serde_json::from_value(json!({
            "model": "m",
            "messages": [{"role": "user", "content": "x"}],
            "tool_choice": {"type": "tool", "name": "Bash"}
        }))
        .unwrap();
        assert!(matches!(
            flatten_anthropic(&input).unwrap().tool_choice,
            Some(ToolChoice::Named(n)) if n == "Bash"
        ));
    }

    #[test]
    fn unknown_block_rejected_and_empty_rejected() {
        let input: AnthropicInput = serde_json::from_value(json!({
            "model": "m",
            "messages": [{"role": "user", "content": [{"type": "wat"}]}]
        }))
        .unwrap();
        assert!(flatten_anthropic(&input).is_err());
        let input: AnthropicInput =
            serde_json::from_value(json!({"model": "m", "messages": []})).unwrap();
        assert!(flatten_anthropic(&input).is_err());
    }
}
