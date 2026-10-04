//! 上游事件流 → 协议帧的公共泵（三协议共用）。
//!
//! - [`TextGate`]:工具信封拦截。启用时（客户端注册了 tools）累计全文，发现
//!   `<tool_call` 标记后只放行前导正文、隐藏信封本体；结束时
//!   `split_text_and_tool` 拆出结构化工具调用。标记未完成的尾部（如 `<too`）
//!   会滞留到下一帧，绝不吐给客户端。
//! - [`pump`]:把 `UpstreamStream` 逐事件翻译成 [`StreamMsg`]；信封在流结束时
//!   统一结算（尾文本 + 工具调用 + finish_reason），保证帧序正确。

use futures::StreamExt;
use serde_json::Value;

use duckai_protocol::{ToolCallEnvelope, split_text_and_tool};
use duckai_types::UpstreamEvent;
use duckai_upstream::UpstreamStream;

use crate::error::ApiErr;

/// 信封开标记（大小写不敏感匹配）。
const MARKER: &[u8] = b"<tool_call";

/// 十六进制随机 24 位（`chatcmpl-`/`msg_`/`resp_`/`call_` 等 id 素材）。
pub fn hex24() -> String {
    let raw = rand::random::<[u8; 12]>();
    let mut s = String::with_capacity(24);
    for b in raw {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// OpenAI completion id：`chatcmpl-{hex24}`，**同一条流内全帧一致**（修正原项目逐帧换 id 的缺陷）。
pub fn completion_id() -> String {
    format!("chatcmpl-{}", hex24())
}

fn find_ci(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).find(|&i| hay[i..i + needle.len()].eq_ignore_ascii_case(needle))
}

/// 流结束时的结算结果。
#[derive(Debug, Clone)]
pub struct Ended {
    /// 信封发现后残留的正文尾（通常为空；信封损坏时把隐藏的全文放出）。
    pub tail: String,
    /// 信封解析出的工具调用（结构化事件优先由调用方合并）。
    pub tool: Option<ToolCallEnvelope>,
    /// 终止 finish_reason（`stop` / `tool_calls` …）。
    pub reason: String,
}

/// 工具信封拦截闸。
pub struct TextGate {
    enabled: bool,
    full: String,
    emitted: usize,
    hidden: bool,
    ended: Option<Ended>,
}

impl TextGate {
    /// `enabled`：客户端是否注册了 tools（决定是否拦截信封 / 结算工具调用）。
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            full: String::new(),
            emitted: 0,
            hidden: false,
            ended: None,
        }
    }

    /// 喂一帧正文，返回**此刻可发放**的文本（信封部分永远不发）。
    pub fn feed(&mut self, delta: &str) -> String {
        if !self.enabled {
            return delta.to_string();
        }
        if self.ended.is_some() {
            return String::new();
        }
        self.full.push_str(delta);
        if self.hidden {
            return String::new();
        }
        // 找到信封开标记（大小写不敏感）→ 放行前导、之后全部隐藏。
        if let Some(off) = find_ci(&self.full.as_bytes()[self.emitted..], MARKER) {
            let pos = self.emitted + off;
            self.hidden = true;
            let out = self.full[self.emitted..pos].to_string();
            self.emitted = pos;
            return out;
        }
        // 未见标记：滞留「可能是标记前缀」的尾巴（最长后缀且为 MARKER 前缀）。
        let bytes = self.full.as_bytes();
        let max = MARKER.len().min(bytes.len().saturating_sub(self.emitted));
        let mut keep = 0usize;
        for n in 1..=max {
            let start = bytes.len() - n;
            if start < self.emitted {
                break;
            }
            if bytes[start..].eq_ignore_ascii_case(&MARKER[..n]) {
                keep = n;
            }
        }
        let flush_end = self.full.len() - keep;
        let out = self.full[self.emitted..flush_end].to_string();
        self.emitted = flush_end;
        out
    }

    /// 流终止结算（幂等；第一次之后返回同一结果）。
    pub fn end(&mut self, reason: impl Into<String>) -> Ended {
        if let Some(e) = &self.ended {
            return e.clone();
        }
        let reason = reason.into();
        let (tail, tool) = if !self.enabled || self.full.is_empty() {
            (String::new(), None)
        } else if self.hidden {
            let (_pre, parsed) = split_text_and_tool(&self.full);
            if parsed.is_some() {
                // 前导正文已全部放行；信封本体丢弃。
                (String::new(), parsed)
            } else {
                // 标记坏了（如只有半截 JSON）→ 把隐藏内容作为纯文本放出。
                (self.full[self.emitted..].to_string(), None)
            }
        } else {
            (self.full[self.emitted..].to_string(), None)
        };
        let e = Ended { tail, tool, reason };
        self.ended = Some(e.clone());
        e
    }
}

/// 泵出的一条协议无关消息。
#[derive(Debug, Clone)]
pub enum StreamMsg {
    Text(String),
    Reasoning(String),
    ToolCall {
        id: String,
        name: String,
        arguments: Value,
    },
    Ping,
    /// 流终止（尾文本 / 信封工具调用 / finish_reason 一并结算）。
    End(Ended),
}

/// 逐事件翻译；`None` 表示上游流结束（理论不可达——`End` 已覆盖 EOF）。
pub async fn pump(
    stream: &mut UpstreamStream,
    gate: &mut TextGate,
) -> Option<Result<StreamMsg, ApiErr>> {
    loop {
        let ev = match stream.next().await {
            None => return Some(Ok(StreamMsg::End(gate.end("stop")))),
            Some(Err(e)) => return Some(Err(e.into())),
            Some(Ok(ev)) => ev,
        };
        match ev {
            UpstreamEvent::TextDelta(t) => {
                let flushed = gate.feed(&t);
                if flushed.is_empty() {
                    continue;
                }
                return Some(Ok(StreamMsg::Text(flushed)));
            }
            UpstreamEvent::ReasoningDelta(r) => return Some(Ok(StreamMsg::Reasoning(r))),
            UpstreamEvent::ToolCall {
                id,
                name,
                arguments,
            } => {
                return Some(Ok(StreamMsg::ToolCall {
                    id,
                    name,
                    arguments,
                }));
            }
            UpstreamEvent::Ping => return Some(Ok(StreamMsg::Ping)),
            UpstreamEvent::Source { .. }
            | UpstreamEvent::Title(_)
            | UpstreamEvent::ToolResult { .. } => continue,
            UpstreamEvent::Done { finish_reason } => {
                return Some(Ok(StreamMsg::End(gate.end(finish_reason))));
            }
        }
    }
}

/// 非流式收集结果。
#[derive(Debug, Clone)]
pub struct Collected {
    pub text: String,
    /// 结构化工具事件（上游 tool-invocation 帧）。
    pub tool_event: Option<(String, String, Value)>,
    /// 信封工具调用。
    pub tool_envelope: Option<ToolCallEnvelope>,
    pub reason: String,
}

impl Collected {
    /// 合并后的唯一工具调用（结构化事件优先）。
    pub fn tool(&self) -> Option<(String, String, Value)> {
        if let Some(t) = &self.tool_event {
            return Some(t.clone());
        }
        self.tool_envelope.as_ref().map(|e| {
            (
                format!("call_{}", hex24()),
                e.name.clone(),
                Value::Object(e.input.clone()),
            )
        })
    }
}

/// 非流式消费整条流（信封闸按 `tools_enabled` 启停）。
pub async fn collect(
    stream: &mut UpstreamStream,
    tools_enabled: bool,
) -> Result<Collected, ApiErr> {
    let mut gate = TextGate::new(tools_enabled);
    let mut text = String::new();
    let mut tool_event = None;
    loop {
        match pump(stream, &mut gate).await {
            None => break,
            Some(Err(e)) => return Err(e),
            Some(Ok(msg)) => match msg {
                StreamMsg::Text(t) => text.push_str(&t),
                StreamMsg::Reasoning(_) => {}
                StreamMsg::ToolCall {
                    id,
                    name,
                    arguments,
                } => tool_event = Some((id, name, arguments)),
                StreamMsg::Ping => {}
                StreamMsg::End(e) => {
                    text.push_str(&e.tail);
                    let reason = e.reason;
                    return Ok(Collected {
                        text,
                        tool_event,
                        tool_envelope: e.tool,
                        reason,
                    });
                }
            },
        }
    }
    // 理论不可达（EOF 已在 pump 内结算）；兜底一次。
    let ended = gate.end("stop");
    text.push_str(&ended.tail);
    Ok(Collected {
        text,
        tool_event,
        tool_envelope: ended.tool,
        reason: ended.reason,
    })
}
