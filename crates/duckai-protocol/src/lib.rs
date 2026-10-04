//! # duckai-protocol — 协议层
//!
//! Duck.ai 上游协议的**纯构造与解析**：请求体、请求头、VQD 生命周期、挑战求解（rquickjs）、
//! 上游 SSE 解析、三协议输入扁平化、tool 信封与意图路由。
//!
//! 分层约束（ARCHITECTURE.md §3）：
//! - 本 crate **不做 I/O 派发**（无 reqwest / axum）——取挑战、发问、抓首页等网络动作
//!   由 `duckai-upstream` 驱动，本层只负责“字节进/字节出”的确定性转换；
//! - 依赖方向单向：`duckai-server → duckai-api → duckai-upstream → duckai-protocol → duckai-types`。
//!
//! ## 版权与再许可
//!
//! `pow` 模块中的 DOM 桩（prelude.js）与 mutation.js 改编自
//! [aurora-develop/Duck2api](https://github.com/aurora-develop/Duck2api)
//! （MIT License，Copyright (c) 2024 aurora-develop），Go 实现的行为语义亦参考该项目。
//! 其余协议改写自 DuckAI2API（MIT，Copyright (c) 2026 Chen Zhenpeng）。

pub mod chat;
pub mod flatten;
pub mod headers;
pub mod home;
pub mod pow;
pub mod sse;
pub mod tool_envelope;
pub mod tool_router;
pub mod vqd;

pub use chat::{
    DuckChatBody, RsaJwk, StickyConversation, build_chat_body, build_chat_body_once,
    generate_encryption_jwk,
};
pub use flatten::{
    FlattenError, Flattened, flatten_anthropic, flatten_conversation, flatten_openai,
    flatten_responses,
};
pub use headers::{build_chat_headers, build_status_headers, fe_signals, journey_id};
pub use home::{DEFAULT_ENTRY_BUNDLE_HASH, DEFAULT_FE_VERSION, FeMeta, parse_home};
pub use pow::{PowEngine, PowEnv, PowError, PowSolution, RquickjsPow};
pub use sse::{SseParser, UpstreamFrame};
pub use tool_envelope::{
    ToolCallEnvelope, parse_tool_call, render_tools_prompt, split_text_and_tool,
};
pub use tool_router::{KNOWN_TOOLS, has_tool_result, route_from_turns, route_intent};
pub use vqd::VqdStore;
