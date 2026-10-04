//! 上游 `/duckchat/v1/chat` SSE 流的增量解析（§2.2.7）。
//!
//! 纯字节解析，无 I/O：上游层把响应体 chunk 喂进 [`SseParser`]，得到裸 `data:` 载荷，
//! 再用 [`decode_frame`] 翻成协议中立的 [`UpstreamEvent`]（API 层再转三协议帧）。
//!
//! 帧语义（线上实测 + 参考实现）：
//! - `data: {json}` 且 `action:"success"` → 正文增量；
//! - `data: {"action":"error","type":"ERR_*"}` → 挑战/限流/封禁错误帧（HTTP 常仍为 200）；
//! - `data: [DONE] / [PING] / [CHAT_TITLE:..]` → 结束 / 心跳 / 会话标题；
//! - `role:"tool-invocation"` → 工具调用（relay 信封语法）。

use serde::Deserialize;
use serde_json::Value;

use duckai_types::UpstreamEvent;

/// 解析后的一帧（含上游错误帧——它与正文同流，HTTP 状态码不能表达封禁/挑战）。
#[derive(Debug, Clone, PartialEq)]
pub enum UpstreamFrame {
    Event(UpstreamEvent),
    /// 上游内联错误：`status`（若给出）、`type`（`ERR_CHALLENGE` 等）、`overrideCode`。
    Error {
        status: Option<u16>,
        kind: String,
        override_code: Option<String>,
    },
    /// 非正文帧（事件 id 行、空数据、未知标签）——直接忽略。
    Ignore,
}

/// 增量 SSE 解析器：按空行切帧，支持任意切片喂入（含逐字节）。
#[derive(Debug, Default)]
pub struct SseParser {
    buf: String,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂入一段字节（须为 UTF-8 文本），返回本次完成的全部 `data:` 载荷。
    pub fn push(&mut self, chunk: &str) -> Vec<String> {
        self.buf.push_str(&chunk.replace("\r\n", "\n"));
        let mut out = Vec::new();
        while let Some(idx) = self.buf.find("\n\n") {
            let block: String = self.buf.drain(..idx + 2).collect();
            if let Some(data) = extract_data(&block) {
                out.push(data);
            }
        }
        out
    }

    /// 流结束时冲洗缓冲（上游若未以空行收尾，最后一帧也解析出来）。
    pub fn flush(&mut self) -> Vec<String> {
        if self.buf.is_empty() {
            return Vec::new();
        }
        let block = std::mem::take(&mut self.buf);
        extract_data(&block).into_iter().collect()
    }
}

/// 从一个 SSE 事件块中提取拼接后的 `data:` 载荷（SSE 规范：多条 data 行以 \n 连接）。
fn extract_data(block: &str) -> Option<String> {
    let mut lines = Vec::new();
    for line in block.split('\n') {
        let line = line.trim_end_matches('\n');
        if let Some(rest) = line.strip_prefix("data:") {
            lines.push(rest.strip_prefix(' ').unwrap_or(rest));
        }
    }
    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

#[derive(Debug, Deserialize)]
struct Wire {
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    status: Option<u16>,
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default, rename = "overrideCode")]
    override_code: Option<String>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default, rename = "toolName")]
    tool_name: Option<String>,
    #[serde(default, rename = "toolArguments")]
    tool_arguments: Option<Value>,
    #[serde(default, rename = "toolResult")]
    tool_result: Option<Value>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    title: Option<String>,
}

/// 把一条 `data:` 载荷翻译为帧。
pub fn decode_frame(data: &str) -> UpstreamFrame {
    let data = data.trim();
    if data.is_empty() {
        return UpstreamFrame::Ignore;
    }
    if data == "[DONE]" {
        return UpstreamFrame::Event(UpstreamEvent::Done {
            finish_reason: "stop".into(),
        });
    }
    if data == "[PING]" {
        return UpstreamFrame::Event(UpstreamEvent::Ping);
    }
    if let Some(title) = data.strip_prefix("[CHAT_TITLE:") {
        let title = title.strip_suffix(']').unwrap_or(title);
        if !title.is_empty() {
            return UpstreamFrame::Event(UpstreamEvent::Title(title.to_string()));
        }
        return UpstreamFrame::Ignore;
    }
    if data.starts_with('[') {
        // 其它 [NOTE:...] 之类旁路标签
        return UpstreamFrame::Ignore;
    }

    let Ok(wire) = serde_json::from_str::<Wire>(data) else {
        return UpstreamFrame::Ignore;
    };

    // ---- 错误帧（挑战 / 限流 / 封禁） ----
    let is_err = wire.action.as_deref() == Some("error")
        || wire.kind.as_deref().is_some_and(|k| k.starts_with("ERR_"));
    if is_err {
        return UpstreamFrame::Error {
            status: wire.status,
            kind: wire
                .kind
                .or_else(|| wire.action.clone())
                .unwrap_or_else(|| "ERR_UNKNOWN".into()),
            override_code: wire.override_code,
        };
    }

    // ---- 工具调用 ----
    if wire.role.as_deref() == Some("tool-invocation") {
        match wire.state.as_deref() {
            Some("call") => {
                let name = wire.tool_name.unwrap_or_default();
                if name.is_empty() {
                    return UpstreamFrame::Ignore;
                }
                return UpstreamFrame::Event(UpstreamEvent::ToolCall {
                    id: wire.id.unwrap_or_default(),
                    name,
                    arguments: wire.tool_arguments.unwrap_or(Value::Null),
                });
            }
            Some("result") => {
                let content = match wire.tool_result {
                    Some(Value::String(s)) => s,
                    Some(other) => other.to_string(),
                    None => wire.message.unwrap_or_default(),
                };
                return UpstreamFrame::Event(UpstreamEvent::ToolResult {
                    id: wire.id.unwrap_or_default(),
                    name: wire.tool_name.unwrap_or_default(),
                    content,
                    is_error: false,
                });
            }
            _ => return UpstreamFrame::Ignore,
        }
    }

    // ---- 引用来源 ----
    if wire.role.as_deref() == Some("source") {
        let url = wire.url.unwrap_or_default();
        if url.is_empty() {
            return UpstreamFrame::Ignore;
        }
        return UpstreamFrame::Event(UpstreamEvent::Source {
            url,
            title: wire.title.unwrap_or_default(),
        });
    }

    // ---- 推理 / 正文增量 ----
    if let Some(reasoning) = wire.reasoning.filter(|s| !s.is_empty()) {
        if wire.message.as_deref().is_none_or(str::is_empty) {
            return UpstreamFrame::Event(UpstreamEvent::ReasoningDelta(reasoning));
        }
    }
    if let Some(message) = wire.message.filter(|s| !s.is_empty()) {
        return UpstreamFrame::Event(UpstreamEvent::TextDelta(message));
    }

    UpstreamFrame::Ignore
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_captured_success_stream() {
        let raw = include_str!("../../../fixtures/sse_success.txt");
        let mut parser = SseParser::new();
        let mut frames = Vec::new();
        for payload in parser.push(raw) {
            frames.push(decode_frame(&payload));
        }
        frames.extend(parser.flush().iter().map(|p| decode_frame(p)));
        assert_eq!(
            frames,
            vec![
                UpstreamFrame::Event(UpstreamEvent::TextDelta("P".into())),
                UpstreamFrame::Event(UpstreamEvent::TextDelta("ONG".into())),
                UpstreamFrame::Event(UpstreamEvent::Done {
                    finish_reason: "stop".into()
                }),
            ]
        );
    }

    #[test]
    fn byte_at_a_time_feeding_is_stable() {
        let raw = include_str!("../../../fixtures/sse_success.txt");
        let mut parser = SseParser::new();
        let mut payloads = Vec::new();
        for ch in raw.chars() {
            payloads.extend(parser.push(&ch.to_string()));
        }
        payloads.extend(parser.flush());
        assert_eq!(payloads.len(), 3);
        assert_eq!(payloads[2], "[DONE]");
    }

    #[test]
    fn decodes_challenge_and_ban_error_frames() {
        let challenge = include_str!("../../../fixtures/sse_error_challenge.txt").trim();
        assert_eq!(
            decode_frame(challenge),
            UpstreamFrame::Error {
                status: Some(418),
                kind: "ERR_CHALLENGE".into(),
                override_code: Some("3501".into()),
            }
        );
        let ban = include_str!("../../../fixtures/sse_error_ban.txt").trim();
        assert_eq!(
            decode_frame(ban),
            UpstreamFrame::Error {
                status: Some(418),
                kind: "ERR_BN_LIMIT".into(),
                override_code: Some("5b5b".into()),
            }
        );
    }

    #[test]
    fn decodes_tool_source_ping_title() {
        assert_eq!(
            decode_frame(
                r#"{"id":"m1","role":"tool-invocation","state":"call","toolName":"Read","toolArguments":{"file_path":"/tmp/x"}}"#
            ),
            UpstreamFrame::Event(UpstreamEvent::ToolCall {
                id: "m1".into(),
                name: "Read".into(),
                arguments: serde_json::json!({"file_path": "/tmp/x"}),
            })
        );
        assert_eq!(
            decode_frame(r#"{"role":"source","url":"https://example.com","title":"Example"}"#),
            UpstreamFrame::Event(UpstreamEvent::Source {
                url: "https://example.com".into(),
                title: "Example".into(),
            })
        );
        assert_eq!(
            decode_frame("[PING]"),
            UpstreamFrame::Event(UpstreamEvent::Ping)
        );
        assert_eq!(
            decode_frame("[CHAT_TITLE:会话一]"),
            UpstreamFrame::Event(UpstreamEvent::Title("会话一".into()))
        );
        assert_eq!(decode_frame("[NOTE:something]"), UpstreamFrame::Ignore);
    }

    #[test]
    fn reasoning_and_empty_message() {
        assert_eq!(
            decode_frame(r#"{"role":"assistant","reasoning":"thinking..."}"#),
            UpstreamFrame::Event(UpstreamEvent::ReasoningDelta("thinking...".into()))
        );
        assert_eq!(
            decode_frame(r#"{"role":"assistant","message":""}"#),
            UpstreamFrame::Ignore
        );
        assert_eq!(decode_frame("event: ping"), UpstreamFrame::Ignore);
        assert_eq!(decode_frame("not json at all"), UpstreamFrame::Ignore);
    }

    #[test]
    fn multi_data_lines_join_with_newline() {
        let mut p = SseParser::new();
        let payloads = p.push("data: a\ndata: b\n\n");
        assert_eq!(payloads, vec!["a\nb".to_string()]);
    }
}
