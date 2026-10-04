use serde_json::Value;

/// 上游 SSE 解析后的协议中立事件；API 层把同一条事件流翻译成
/// OpenAI `chat.completion.chunk`、Anthropic `content_block_*`、
/// Responses `response.output_text.delta` 三种帧。
#[derive(Debug, Clone, PartialEq)]
pub enum UpstreamEvent {
    /// 正文增量（流式，逐 chunk；修正 P1-5 假流式）。
    TextDelta(String),
    /// 推理/思考增量（若上游给出）。
    ReasoningDelta(String),
    /// 工具调用（信封解析或上游结构化输出）。
    ToolCall {
        id: String,
        name: String,
        arguments: Value,
    },
    /// 工具结果回传（relay 侧合成，服务端绝不执行工具）。
    ToolResult {
        id: String,
        name: String,
        content: String,
        is_error: bool,
    },
    /// 引用来源。
    Source { url: String, title: String },
    /// 心跳（Anthropic ping 帧的来源）。
    Ping,
    /// 会话标题（非正文，丢弃或记录）。
    Title(String),
    /// 流结束。
    Done { finish_reason: String },
}

impl UpstreamEvent {
    /// 是否为终止事件。
    pub fn is_done(&self) -> bool {
        matches!(self, UpstreamEvent::Done { .. })
    }
}
