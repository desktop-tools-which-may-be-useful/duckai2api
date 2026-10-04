//! 零依赖基础类型：ID、错误、事件、模型、只读状态快照。
//!
//! 依赖方向的最底层，禁止反向依赖任何 crate。

pub mod error;
pub mod event;
pub mod model;
pub mod snapshot;
pub mod turn;

pub use error::{EgressScope, UpstreamError};
pub use event::UpstreamEvent;
pub use model::{ModelInfo, ModelSource};
pub use snapshot::{
    AdminControl, AdminState, EgressHealth, EgressSnapshot, HealthSnapshot, LogEntry,
    SettingsSnapshot, UpstreamHealth,
};
pub use turn::{ChatTurn, ContentBlock, Role, ToolChoice, ToolDef, TurnContent};

/// 一次对话请求（协议无关，已由 duckai-protocol 扁平化）。
#[derive(Debug, Clone)]
pub struct UpstreamRequest {
    pub model: String,
    pub turns: Vec<ChatTurn>,
    /// 仅 BUILT-IN 工具集（存在时渲染工具信封并置 canUseTools）。
    pub tool_choice: Option<ToolChoice>,
    pub tools: Vec<ToolDef>,
    pub reasoning_effort: Option<String>,
    /// 同会话粘性（选代理 / VQD、conversationId 复用）。
    pub session_hint: Option<String>,
}

impl UpstreamRequest {
    pub fn new(model: impl Into<String>, turns: Vec<ChatTurn>) -> Self {
        Self {
            model: model.into(),
            turns,
            tool_choice: None,
            tools: Vec::new(),
            reasoning_effort: None,
            session_hint: None,
        }
    }
}
