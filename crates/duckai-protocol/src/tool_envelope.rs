//! 工具信封（§1.3）：Duck.ai 无原生 function tools，靠提示词注入教学信封。
//!
//! 与 Python `tools.py` 的差异（P1 修正）：
//! - 解析顺序改为 **先切闭合标记、再剥 markdown 围栏、最后 JSON 解析**——原实现先剥围栏
//!   会在围栏带 `` ``` `` 结束且信封被截断时把损坏 JSON 当成实参（broken-args）；
//! - 兜底形态：JSON 合法但不是对象 → `{"input": <原始>}`；完全非 JSON → `{"input": raw}`。

use serde_json::{Map, Value};

use duckai_types::ToolDef;

/// 解析出的一次工具调用。
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCallEnvelope {
    pub name: String,
    pub input: Map<String, Value>,
}

/// 渲染系统提示后缀：教学信封语法 + 每个工具的 schema（Python `render_tools_prompt` 对齐）。
pub fn render_tools_prompt(tools: &[ToolDef]) -> String {
    if tools.is_empty() {
        return String::new();
    }
    let mut lines = vec![
        String::new(),
        "## TOOL USE".to_string(),
        "You have access to tools. To call one, emit EXACTLY ONE XML envelope on its own,"
            .to_string(),
        "with the arguments as a JSON object (no markdown fences):".to_string(),
        String::new(),
        "<tool_call name=\"tool_name\">{ \"arg\": \"value\" }>".to_string(),
        String::new(),
        "Rules:".to_string(),
        "- Emit the envelope only when you need a tool; otherwise reply in plain text.".to_string(),
        "- Do not wrap the JSON in ``` or explain the call. Just the envelope.".to_string(),
        "- Use valid JSON matching the schema. Available tools:".to_string(),
        String::new(),
    ];
    for t in tools {
        lines.push(format!("### {}", t.name));
        if let Some(desc) = t.description.as_deref() {
            let desc = desc.trim();
            if !desc.is_empty() {
                lines.push(desc.to_string());
            }
        }
        lines.push(format!(
            "input_schema: {}",
            serde_json::to_string(&t.input_schema).unwrap_or_else(|_| "{}".into())
        ));
        lines.push(String::new());
    }
    lines.join("\n")
}

/// 找到信封起点（开标签结束处）与名字。
fn find_open(text: &str) -> Option<(usize, String)> {
    let lower = text.to_ascii_lowercase();
    let start = lower.find("<tool_call")?;
    let after = &text[start..];
    let end_rel = after.find('>')?;
    let tag = &after[..end_rel];
    let name_part = tag.split_once("name=").map(|(_, v)| v)?;
    let name = name_part
        .trim()
        .trim_start_matches('"')
        .trim_start_matches('\'')
        .split('"')
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    if name.is_empty() {
        return None;
    }
    Some((start + end_rel + 1, name))
}

/// 信封正文截取：优先按 JSON 结构平衡（容忍字符串内 `>` 与围栏），兜底首个 `>` / 闭合标记。
fn body_after(rest: &str) -> &str {
    let t = rest.trim_start_matches(['\n', '\r', '\t', ' ']);
    if let Some(end) = extract_json_end(t) {
        return &t[..end];
    }
    if t.starts_with("```") {
        // 围栏包裹但非 JSON：切到最后一个围栏结束标记
        if let Some(i) = t.rfind("```") {
            if i > 0 {
                return &t[..i];
            }
        }
        return t;
    }
    // 兜底：取 "```"、"</"（闭合标记）与 ">" 中最早出现者，避免把 `</tool_call`
    // 整段留在正文里（P1 broken-args 修正的姊妹分支）
    [rest.find("```"), rest.find("</"), rest.find('>')]
        .into_iter()
        .flatten()
        .min()
        .map_or(rest, |i| &rest[..i])
}

/// 括号平衡扫描（跳过字符串与转义），返回第一个完整 JSON 值的结束位置。
fn extract_json_end(s: &str) -> Option<usize> {
    let mut depth: i32 = 0;
    let mut in_str = false;
    let mut esc = false;
    let mut started = false;
    for (i, c) in s.char_indices() {
        if in_str {
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_str = true;
                started = true;
            }
            '{' | '[' => {
                depth += 1;
                started = true;
            }
            '}' | ']' => {
                depth -= 1;
                if started && depth == 0 {
                    return Some(i + c.len_utf8());
                }
            }
            _ => {}
        }
        if !started && c == '>' {
            return None; // JSON 之前就出现 > → 走兜底逻辑
        }
    }
    None
}

/// 剥掉可能包裹 JSON 的 markdown 围栏（```json / ```）。
fn strip_fences(raw: &str) -> &str {
    let mut s = raw.trim();
    if s.starts_with("```") {
        s = &s[3..];
        if s.len() >= 4 && s[..4].eq_ignore_ascii_case("json") {
            s = &s[4..];
        }
        s = s.trim_start_matches([' ', '\t', '\n', '\r']);
    }
    let t = s.trim_end();
    if let Some(stripped) = t.strip_suffix("```") {
        s = stripped.trim_end();
    }
    s
}

/// 解析模型输出中的第一个信封；纯文本回复返回 `None`。
pub fn parse_tool_call(text: &str) -> Option<ToolCallEnvelope> {
    let (body_start, name) = find_open(text)?;
    let rest = &text[body_start..];
    // P1 修正：先切闭合标记，再剥围栏，最后解析——围栏/闭合混杂时不再产出损坏实参。
    let body = strip_fences(body_after(rest));
    let input = match serde_json::from_str::<Value>(body) {
        Ok(Value::Object(map)) => map,
        Ok(other) => {
            let mut map = Map::new();
            map.insert("input".into(), other);
            map
        }
        Err(_) => {
            let mut map = Map::new();
            map.insert("input".into(), Value::String(body.to_string()));
            map
        }
    };
    Some(ToolCallEnvelope { name, input })
}

/// 拆分正文与信封：`(前导自然语言, 工具调用)`。
pub fn split_text_and_tool(text: &str) -> (String, Option<ToolCallEnvelope>) {
    let Some((body_start, _)) = find_open(text) else {
        return (text.trim().to_string(), None);
    };
    // 回退到开标签起点
    let open_start = text[..body_start].rfind("<tool_call").unwrap_or(body_start);
    let preamble = text[..open_start].trim().to_string();
    let call = parse_tool_call(text);
    (preamble, call)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tools() -> Vec<ToolDef> {
        vec![
            ToolDef {
                name: "Read".into(),
                description: Some("Read a file".into()),
                input_schema: json!({"type": "object"}),
            },
            ToolDef {
                name: "Bash".into(),
                description: None,
                input_schema: json!({}),
            },
        ]
    }

    #[test]
    fn render_lists_tools_and_syntax() {
        let prompt = render_tools_prompt(&tools());
        assert!(prompt.contains("## TOOL USE"));
        assert!(prompt.contains("<tool_call name=\"tool_name\">"));
        assert!(prompt.contains("### Read"));
        assert!(prompt.contains("input_schema: {\"type\":\"object\"}"));
        assert!(prompt.contains("### Bash"));
        assert!(prompt.contains("- Use valid JSON matching the schema."));
        assert_eq!(render_tools_prompt(&[]), "");
    }

    #[test]
    fn round_trip_simple() {
        let prompt = render_tools_prompt(&tools());
        // 渲染出的语法示例本身是合法信封（渲染/解析对称）
        let example =
            parse_tool_call("<tool_call name=\"tool_name\">{ \"arg\": \"value\" }>").unwrap();
        assert_eq!(example.name, "tool_name");
        assert_eq!(example.input["arg"], "value");
        // 模型按提示产出的信封可回读
        let call = parse_tool_call("<tool_call name=\"Read\">{\"file_path\":\"/tmp/x\"}>").unwrap();
        assert_eq!(call.name, "Read");
        assert_eq!(call.input["file_path"], "/tmp/x");
        assert!(prompt.contains("<tool_call name=\"tool_name\">"));
    }

    #[test]
    fn fenced_json_parses_intact() {
        // P1 修正回归：围栏包裹时闭合标记先切、围栏后剥，实参不被破坏
        let raw = "好，我来读。\n<tool_call name=\"Bash\">\n```json\n{\"command\": \"ls -la\"}\n```</tool_call>";
        let (preamble, call) = split_text_and_tool(raw);
        assert_eq!(preamble, "好，我来读。");
        let call = call.unwrap();
        assert_eq!(call.name, "Bash");
        assert_eq!(call.input["command"], "ls -la", "围栏剥离不得截断 JSON");
    }

    #[test]
    fn missing_close_tag_tolerated() {
        let raw = "<tool_call name=\"Read\">{\"file_path\":\"a.txt\"}";
        let call = parse_tool_call(raw).unwrap();
        assert_eq!(call.input["file_path"], "a.txt");
    }

    #[test]
    fn non_object_json_wrapped_as_input() {
        let call = parse_tool_call(r#"<tool_call name="Bash">["a","b"]</tool_call>"#).unwrap();
        assert_eq!(call.input["input"], json!(["a", "b"]));
        let call = parse_tool_call(r#"<tool_call name="Bash">just text</tool_call>"#).unwrap();
        assert_eq!(call.input["input"], json!("just text"));
    }

    #[test]
    fn plain_text_has_no_call() {
        let (t, c) = split_text_and_tool("这里没有工具调用。");
        assert_eq!(t, "这里没有工具调用。");
        assert!(c.is_none());
        assert!(parse_tool_call("").is_none());
        assert!(parse_tool_call("<tool_call name=\"\">{}</tool_call>").is_none());
    }

    #[test]
    fn multiline_preamble_and_json_containing_gt() {
        let raw =
            "第一行\n第二行 <tool_call name=\"Write\">{\"content\": \"a > b\"}</tool_call> 后缀";
        let (preamble, call) = split_text_and_tool(raw);
        assert_eq!(preamble, "第一行\n第二行");
        let call = call.unwrap();
        assert_eq!(
            call.input["content"], "a > b",
            "有 </tool_call> 时不得在首个 > 处截断"
        );
    }
}
