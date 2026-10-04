//! OpenAI Chat Completions 输入扁平化。
//!
//! 覆盖：system / user / assistant（含 `tool_calls` 历史）/ tool 四类消息、
//! `content` 字符串或块数组（text / image_url）、`tools`、`tool_choice` 四态。

use serde::Deserialize;
use serde_json::Value;

use duckai_types::{ChatTurn, ContentBlock, Role, ToolChoice, ToolDef, TurnContent};

use super::{FlattenError, Flattened, err, parse_args};

#[derive(Debug, Deserialize)]
pub struct ChatCompletionsInput {
    pub model: String,
    #[serde(default)]
    pub messages: Vec<OpenAiMessage>,
    #[serde(default)]
    pub tools: Vec<OpenAiTool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct OpenAiMessage {
    pub role: String,
    #[serde(default)]
    pub content: Option<Value>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub tool_call_id: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<OpenAiToolCall>>,
}

#[derive(Debug, Deserialize)]
pub struct OpenAiToolCall {
    pub id: String,
    pub function: OpenAiFunctionCall,
}

#[derive(Debug, Deserialize)]
pub struct OpenAiFunctionCall {
    pub name: String,
    #[serde(default)]
    pub arguments: String,
}

#[derive(Debug, Deserialize)]
pub struct OpenAiTool {
    #[serde(default)]
    pub r#type: Option<String>,
    pub function: OpenAiToolFunction,
}

#[derive(Debug, Deserialize)]
pub struct OpenAiToolFunction {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub parameters: Option<Value>,
}

/// 扁平化一次 `/v1/chat/completions` 输入。
pub fn flatten_openai(input: &ChatCompletionsInput) -> Result<Flattened, FlattenError> {
    if input.messages.is_empty() {
        return Err(err("messages must not be empty"));
    }
    let mut out = Flattened::new();

    for msg in &input.messages {
        let content = turn_content(msg)?;
        // assistant.tool_calls 历史 → ToolUse 块
        let mut blocks = match content {
            TurnContent::Text(t) if msg.tool_calls.as_ref().is_none_or(|c| c.is_empty()) => {
                vec![ContentBlock::Text { text: t }]
            }
            TurnContent::Text(t) => vec![ContentBlock::Text { text: t }],
            TurnContent::Blocks(b) => b,
        };
        if let Some(calls) = &msg.tool_calls {
            for call in calls {
                blocks.push(ContentBlock::ToolUse {
                    id: call.id.clone(),
                    name: call.function.name.clone(),
                    input: parse_args(&call.function.arguments),
                });
            }
        }

        let (role, tool_call_id) = match msg.role.as_str() {
            "system" | "developer" => (Role::System, None),
            "assistant" => (Role::Assistant, None),
            "tool" | "function" => (Role::Tool, msg.tool_call_id.clone()),
            "user" => (Role::User, None),
            other => return Err(err(format!("unknown message role: {other}"))),
        };

        // tool 消息的 content 一定是其结果文本
        if role == Role::Tool && blocks.len() == 1 && matches!(blocks[0], ContentBlock::Text { .. })
        {
            out.turns.push(ChatTurn {
                role,
                content: TurnContent::Blocks(blocks),
                name: msg.name.clone(),
                tool_call_id,
            });
            continue;
        }

        let content = if blocks.len() == 1 {
            match blocks.into_iter().next().unwrap() {
                ContentBlock::Text { text } => TurnContent::Text(text),
                other => TurnContent::Blocks(vec![other]),
            }
        } else {
            TurnContent::Blocks(blocks)
        };
        out.turns.push(ChatTurn {
            role,
            content,
            name: msg.name.clone(),
            tool_call_id,
        });
    }

    out.tools = input
        .tools
        .iter()
        .map(|t| ToolDef {
            name: t.function.name.clone(),
            description: t.function.description.clone(),
            input_schema: t
                .function
                .parameters
                .clone()
                .unwrap_or_else(|| serde_json::json!({})),
        })
        .collect();
    out.tool_choice = input
        .tool_choice
        .as_ref()
        .map(tool_choice_openai)
        .transpose()?;
    Ok(out)
}

fn turn_content(msg: &OpenAiMessage) -> Result<TurnContent, FlattenError> {
    match &msg.content {
        None | Some(Value::Null) => Ok(TurnContent::Text(String::new())),
        Some(Value::String(s)) => Ok(TurnContent::Text(s.clone())),
        Some(Value::Array(parts)) => {
            let mut blocks = Vec::new();
            for part in parts {
                match part.get("type").and_then(Value::as_str) {
                    Some("text") | Some("input_text") | Some("output_text") => {
                        let text = part.get("text").and_then(Value::as_str).unwrap_or_default();
                        blocks.push(ContentBlock::Text {
                            text: text.to_string(),
                        });
                    }
                    Some("image_url") => {
                        let url = part
                            .get("image_url")
                            .and_then(|u| u.get("url"))
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        blocks.push(image_block(url));
                    }
                    Some("image") => {
                        let url = part.get("url").and_then(Value::as_str).unwrap_or_default();
                        blocks.push(image_block(url));
                    }
                    Some(other) => blocks.push(ContentBlock::Text {
                        text: format!("[unsupported part: {other}]"),
                    }),
                    None => {}
                }
            }
            if blocks.is_empty() {
                Ok(TurnContent::Text(String::new()))
            } else {
                Ok(TurnContent::Blocks(blocks))
            }
        }
        Some(_) => Err(err("message content must be string or array")),
    }
}

fn image_block(url: &str) -> ContentBlock {
    if let Some(rest) = url.strip_prefix("data:") {
        let (head, b64) = rest.split_once(',').unwrap_or(("image/png", ""));
        let media_type = head.split(';').next().unwrap_or("image/png").to_string();
        ContentBlock::Image {
            media_type,
            data: b64.to_string(),
        }
    } else {
        // 远程 URL：纯 HTTP 上游不支持，按 §1.3 约定转占位（此处保留 url 供日志）。
        ContentBlock::Text {
            text: format!("[image: {url}]"),
        }
    }
}

fn tool_choice_openai(v: &Value) -> Result<ToolChoice, FlattenError> {
    match v {
        Value::String(s) => match s.as_str() {
            "none" => Ok(ToolChoice::None),
            "auto" => Ok(ToolChoice::Auto),
            "required" => Ok(ToolChoice::Required),
            other => Err(err(format!("unknown tool_choice: {other}"))),
        },
        Value::Object(map) => {
            if let Some(func) = map.get("function").and_then(Value::as_str) {
                return Ok(ToolChoice::Named(func.to_string()));
            }
            if let Some(name) = map
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
            {
                return Ok(ToolChoice::Named(name.to_string()));
            }
            if let Some(name) = map.get("name").and_then(Value::as_str) {
                return Ok(ToolChoice::Named(name.to_string()));
            }
            Err(err("unrecognized tool_choice object"))
        }
        _ => Err(err("tool_choice must be string or object")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn four_message_roles_roundtrip() {
        let input: ChatCompletionsInput = serde_json::from_value(json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "system", "content": "be brief"},
                {"role": "user", "content": "read the file"},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_1", "type": "function", "function": {"name": "Read", "arguments": "{\"file_path\":\"/tmp/x\"}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_1", "content": "file contents"},
                {"role": "user", "content": "thanks"}
            ],
            "tools": [{"type": "function", "function": {"name": "Read", "description": "d", "parameters": {"type": "object"}}}],
            "tool_choice": "auto"
        }))
        .unwrap();
        let flat = flatten_openai(&input).unwrap();
        assert_eq!(
            flat.turns.iter().map(|t| t.role).collect::<Vec<_>>(),
            vec![
                Role::System,
                Role::User,
                Role::Assistant,
                Role::Tool,
                Role::User
            ]
        );
        match &flat.turns[2].content {
            TurnContent::Blocks(blocks) => {
                assert!(
                    blocks
                        .iter()
                        .any(|b| matches!(b, ContentBlock::ToolUse { name, .. } if name == "Read"))
                );
            }
            other => panic!("assistant tool_calls lost: {other:?}"),
        }
        assert_eq!(flat.turns[3].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(flat.tools.len(), 1);
        assert_eq!(flat.tools[0].name, "Read");
        assert!(matches!(flat.tool_choice, Some(ToolChoice::Auto)));
    }

    #[test]
    fn string_and_array_content() {
        let input: ChatCompletionsInput = serde_json::from_value(json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "plain"},
                {"role": "user", "content": [
                    {"type": "text", "text": "a"},
                    {"type": "text", "text": "b"},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,QUJD"}}
                ]}
            ]
        }))
        .unwrap();
        let flat = flatten_openai(&input).unwrap();
        assert_eq!(flat.turns[0].content.text(), "plain");
        match &flat.turns[1].content {
            TurnContent::Blocks(blocks) => {
                assert_eq!(blocks.len(), 3);
                assert!(
                    matches!(&blocks[2], ContentBlock::Image { media_type, data }
                    if media_type == "image/png" && data == "QUJD")
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn tool_choice_four_states() {
        let parse = |tc: Value| tool_choice_openai(&tc);
        assert_eq!(parse(json!("none")).unwrap(), ToolChoice::None);
        assert_eq!(parse(json!("auto")).unwrap(), ToolChoice::Auto);
        assert_eq!(parse(json!("required")).unwrap(), ToolChoice::Required);
        assert_eq!(
            parse(json!({"type": "function", "function": {"name": "Bash"}})).unwrap(),
            ToolChoice::Named("Bash".into())
        );
        assert_eq!(
            parse(json!({"function": {"name": "Grep"}})).unwrap(),
            ToolChoice::Named("Grep".into())
        );
        assert!(parse(json!(123)).is_err());
    }

    #[test]
    fn empty_messages_rejected() {
        let input: ChatCompletionsInput =
            serde_json::from_value(json!({"model": "m", "messages": []})).unwrap();
        assert!(flatten_openai(&input).is_err());
    }

    #[test]
    fn unknown_role_rejected() {
        let input: ChatCompletionsInput = serde_json::from_value(json!({
            "model": "m",
            "messages": [{"role": "wizard", "content": "hi"}]
        }))
        .unwrap();
        assert_eq!(
            flatten_openai(&input),
            Err(FlattenError("unknown message role: wizard".into()))
        );
    }
}
