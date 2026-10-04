//! Responses API 输入扁平化（P0-1 回归焦点：Python 的 `_responses_input_text`
//! 是 NameError 死代码——四种 input 形态在此全部真实解析）。
//!
//! 四形态（OpenAI Responses `input` 合法值）：
//! 1. **纯字符串** → 单条 user 轮；
//! 2. **`{role, content}` 消息列表**（chat 风格，content 为字符串或块数组）；
//! 3. **`{type:"message", role, content:[{type:"input_text"|"output_text"|"text", text}]}`** 结构化消息项；
//! 4. **工具项**：`function_call`（assistant 工具调用）+ `function_call_output`（tool 结果）。
//!
//! 另接受 `instructions`（→ system 轮）与 `reasoning` 项（跳过）。

use serde::Deserialize;
use serde_json::Value;

use duckai_types::{ChatTurn, ContentBlock, Role, ToolChoice, ToolDef, TurnContent};

use super::{FlattenError, Flattened, err, parse_args};

#[derive(Debug, Deserialize)]
pub struct ResponsesInput {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub input: Option<Value>,
    #[serde(default)]
    pub instructions: Option<String>,
    #[serde(default)]
    pub tools: Vec<ResponsesTool>,
    #[serde(default)]
    pub tool_choice: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub stream: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct ResponsesTool {
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub parameters: Option<Value>,
}

/// 扁平化一次 `/v1/responses` 输入。
pub fn flatten_responses(input: &ResponsesInput) -> Result<Flattened, FlattenError> {
    let mut out = Flattened::new();

    if let Some(instructions) = &input.instructions {
        if !instructions.trim().is_empty() {
            out.turns
                .push(ChatTurn::text(Role::System, instructions.clone()));
        }
    }

    match &input.input {
        None => {}
        Some(Value::String(s)) => {
            // 形态 1：纯字符串
            if !s.trim().is_empty() {
                out.turns.push(ChatTurn::text(Role::User, s.clone()));
            }
        }
        Some(Value::Array(items)) => {
            // 形态 2/3/4：消息项 + 工具项 + reasoning 混排
            for item in items {
                push_item(&mut out, item)?;
            }
        }
        Some(obj @ Value::Object(_)) => {
            // 单消息对象（容错）
            push_item(&mut out, obj)?;
        }
        Some(_) => return Err(err("input must be string or array")),
    }

    if out.turns.is_empty() {
        return Err(err("input produced no content"));
    }

    out.tools = input
        .tools
        .iter()
        .filter_map(|t| {
            t.name.as_ref().map(|name| ToolDef {
                name: name.clone(),
                description: t.description.clone(),
                input_schema: t
                    .parameters
                    .clone()
                    .unwrap_or_else(|| serde_json::json!({})),
            })
        })
        .collect();
    out.tool_choice = input
        .tool_choice
        .as_ref()
        .map(tool_choice_responses)
        .transpose()?;
    Ok(out)
}

fn push_item(out: &mut Flattened, item: &Value) -> Result<(), FlattenError> {
    let obj = item
        .as_object()
        .ok_or_else(|| err("input item must be an object"))?;

    // 形态 2：chat 风格 {role, content}
    if obj.contains_key("role") && !obj.contains_key("type") {
        let role = role_of(obj.get("role"))?;
        return push_content(out, role, obj.get("content").unwrap_or(&Value::Null));
    }

    let kind = obj
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| err("input item missing type"))?;
    match kind {
        "message" => {
            let role = role_of(obj.get("role"))?;
            push_content(out, role, obj.get("content").unwrap_or(&Value::Null))
        }
        "function_call" => {
            let id = obj
                .get("call_id")
                .or_else(|| obj.get("id"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let name = obj
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let arguments = obj
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or_default();
            out.turns.push(ChatTurn {
                role: Role::Assistant,
                content: TurnContent::Blocks(vec![ContentBlock::ToolUse {
                    id,
                    name,
                    input: parse_args(arguments),
                }]),
                name: None,
                tool_call_id: None,
            });
            Ok(())
        }
        "function_call_output" => {
            let id = obj
                .get("call_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let output = match obj.get("output") {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Array(items)) => items
                    .iter()
                    .filter_map(|i| {
                        i.get("text")
                            .or_else(|| i.get("content"))
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
                Some(other) => other.to_string(),
                None => String::new(),
            };
            out.turns.push(ChatTurn {
                role: Role::Tool,
                content: TurnContent::Text(output),
                name: None,
                tool_call_id: Some(id),
            });
            Ok(())
        }
        "reasoning" | "item_reference" => Ok(()),
        other => Err(err(format!("unknown input item type: {other}"))),
    }
}

fn push_content(out: &mut Flattened, role: Role, content: &Value) -> Result<(), FlattenError> {
    match content {
        Value::String(s) => {
            if !s.trim().is_empty() {
                out.turns.push(ChatTurn::text(role, s.clone()));
            }
            Ok(())
        }
        Value::Array(blocks) => {
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("input_text") | Some("output_text") | Some("text") => {
                        let text = block
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        if !text.trim().is_empty() {
                            out.turns.push(ChatTurn::text(role, text));
                        }
                    }
                    Some("input_image") | Some("image") => {
                        let url = block
                            .get("image_url")
                            .or_else(|| block.get("url"))
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        out.turns.push(ChatTurn {
                            role,
                            content: TurnContent::Blocks(vec![ContentBlock::Text {
                                text: format!("[image: {url}]"),
                            }]),
                            name: None,
                            tool_call_id: None,
                        });
                    }
                    Some("refusal") => {}
                    Some(other) => {
                        return Err(err(format!("unknown content block type: {other}")));
                    }
                    None => return Err(err("content block missing type")),
                }
            }
            Ok(())
        }
        Value::Null => Ok(()),
        _ => Err(err("content must be string or array")),
    }
}

fn role_of(v: Option<&Value>) -> Result<Role, FlattenError> {
    match v.and_then(Value::as_str) {
        Some("user") => Ok(Role::User),
        Some("system") | Some("developer") => Ok(Role::System),
        Some("assistant") => Ok(Role::Assistant),
        Some("tool") => Ok(Role::Tool),
        other => Err(err(format!("unknown role: {other:?}"))),
    }
}

fn tool_choice_responses(v: &Value) -> Result<ToolChoice, FlattenError> {
    match v {
        Value::String(s) => match s.as_str() {
            "auto" => Ok(ToolChoice::Auto),
            "none" => Ok(ToolChoice::None),
            "required" => Ok(ToolChoice::Required),
            other => Err(err(format!("unknown tool_choice: {other}"))),
        },
        Value::Object(map) => {
            let name = map
                .get("name")
                .and_then(Value::as_str)
                .or_else(|| {
                    map.get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(Value::as_str)
                })
                .ok_or_else(|| err("tool_choice object requires name"))?;
            Ok(ToolChoice::Named(name.to_string()))
        }
        _ => Err(err("tool_choice must be string or object")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---- P0-1 回归：四种 input 形态必须真实可解析 ----

    #[test]
    fn shape_1_bare_string() {
        let input: ResponsesInput =
            serde_json::from_value(json!({"model": "gpt-5.6-luna", "input": "hello"})).unwrap();
        let flat = flatten_responses(&input).unwrap();
        assert_eq!(flat.turns.len(), 1);
        assert_eq!(flat.turns[0].role, Role::User);
        assert_eq!(flat.turns[0].content.text(), "hello");
    }

    #[test]
    fn shape_2_chat_style_message_list() {
        let input: ResponsesInput = serde_json::from_value(json!({
            "model": "m",
            "input": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "hello"},
                {"role": "user", "content": "again"}
            ]
        }))
        .unwrap();
        let flat = flatten_responses(&input).unwrap();
        assert_eq!(
            flat.turns.iter().map(|t| t.role).collect::<Vec<_>>(),
            vec![Role::User, Role::Assistant, Role::User]
        );
        assert_eq!(flat.turns[1].content.text(), "hello");
    }

    #[test]
    fn shape_3_structured_message_items() {
        let input: ResponsesInput = serde_json::from_value(json!({
            "model": "m",
            "instructions": "sys line",
            "input": [
                {"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "question"}
                ]},
                {"type": "message", "role": "assistant", "content": [
                    {"type": "output_text", "text": "answer"}
                ]}
            ]
        }))
        .unwrap();
        let flat = flatten_responses(&input).unwrap();
        assert_eq!(flat.turns[0].role, Role::System, "instructions → system 轮");
        assert_eq!(flat.turns[0].content.text(), "sys line");
        assert_eq!(flat.turns[1].content.text(), "question");
        assert_eq!(flat.turns[2].content.text(), "answer");
    }

    #[test]
    fn shape_4_tool_items() {
        let input: ResponsesInput = serde_json::from_value(json!({
            "model": "m",
            "input": [
                {"type": "function_call", "call_id": "c1", "name": "Bash", "arguments": "{\"command\":\"ls\"}"},
                {"type": "function_call_output", "call_id": "c1", "output": "file1\nfile2"},
                {"type": "reasoning", "summary": [{"type": "summary_text", "text": "thinking"}]},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "done?"}]}
            ],
            "tools": [{"type": "function", "name": "Bash", "description": "shell", "parameters": {"type": "object"}}],
            "tool_choice": "auto"
        }))
        .unwrap();
        let flat = flatten_responses(&input).unwrap();
        assert_eq!(
            flat.turns.iter().map(|t| t.role).collect::<Vec<_>>(),
            vec![Role::Assistant, Role::Tool, Role::User],
            "reasoning 项被跳过"
        );
        match &flat.turns[0].content {
            TurnContent::Blocks(blocks) => match &blocks[0] {
                ContentBlock::ToolUse { id, name, input } => {
                    assert_eq!(id, "c1");
                    assert_eq!(name, "Bash");
                    assert_eq!(input["command"], "ls");
                }
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
        assert_eq!(flat.turns[1].tool_call_id.as_deref(), Some("c1"));
        assert_eq!(flat.turns[1].content.text(), "file1\nfile2");
        assert_eq!(flat.tools[0].name, "Bash");
        assert!(matches!(flat.tool_choice, Some(ToolChoice::Auto)));
    }

    #[test]
    fn tool_choice_function_object() {
        let input: ResponsesInput = serde_json::from_value(json!({
            "model": "m",
            "input": "x",
            "tool_choice": {"type": "function", "name": "Read"}
        }))
        .unwrap();
        assert!(matches!(
            flatten_responses(&input).unwrap().tool_choice,
            Some(ToolChoice::Named(n)) if n == "Read"
        ));
    }

    #[test]
    fn empty_input_rejected() {
        let input: ResponsesInput =
            serde_json::from_value(json!({"model": "m", "input": []})).unwrap();
        assert!(flatten_responses(&input).is_err());
        let input: ResponsesInput = serde_json::from_value(json!({"model": "m"})).unwrap();
        assert!(flatten_responses(&input).is_err());
    }
}
