//! API 层错误分类与三协议规范错误帧（ARCHITECTURE §4 / §7）。
//!
//! - `RateLimited{retry_after}` → 429 + `retry-after`
//! - `Banned` → 503 + `retry-after`（措辞禁止出现「账号被封」，上游匿名按 IP/指纹判定）
//! - `ChallengeFailed` → 503 + `retry-after`
//! - `ModelNotFound` → 404；`InvalidInput` → 400；`Upstream`/`Timeout`/`Transport` → 502
//! - 全局并发闸门取不到令牌 → 429 + `retry-after`（而不是把请求砸向上游）

use axum::http::StatusCode;
use duckai_types::UpstreamError;
use duckai_upstream::INITIAL_BAN_SECS;
use serde_json::Value;

/// 协议族：决定错误帧的序列化形状。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proto {
    OpenAi,
    Anthropic,
    Responses,
}

/// API 层统一错误分类。
#[derive(Debug)]
pub enum ApiErr {
    /// 400：请求形态 / 空提示词 / 扁平化失败。
    Invalid(String),
    /// 401：Bearer 缺失或不匹配。
    Unauthorized,
    /// 429：全局并发闸门耗尽（`retry-after` 秒）。
    Busy(u64),
    /// 429：上游限流。
    RateLimited(u64),
    /// 503：出口 IP/指纹被上游限制（冷却中）。
    Banned(u64),
    /// 503：挑战求解/重取耗尽。
    Challenge(String),
    /// 404：本地模型目录未命中（不透传上游）。
    ModelNotFound(String),
    /// 502：上游非 2xx/非预期响应。
    Upstream { status: u16, detail: String },
    /// 502：超时。
    Timeout,
    /// 502：传输失败。
    Transport(String),
}

impl From<UpstreamError> for ApiErr {
    fn from(e: UpstreamError) -> Self {
        match e {
            UpstreamError::RateLimited { retry_after } => ApiErr::RateLimited(retry_after),
            UpstreamError::Banned { .. } => ApiErr::Banned(INITIAL_BAN_SECS),
            UpstreamError::ChallengeFailed => ApiErr::Challenge("挑战求解退化链耗尽".to_string()),
            UpstreamError::ModelNotFound(m) => ApiErr::ModelNotFound(m),
            UpstreamError::InvalidInput(m) => ApiErr::Invalid(m),
            UpstreamError::Upstream { status, body } => ApiErr::Upstream {
                status,
                detail: truncate(&body, 400),
            },
            UpstreamError::Timeout => ApiErr::Timeout,
            UpstreamError::Transport(m) => ApiErr::Transport(m),
        }
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        let mut end = n;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

impl ApiErr {
    pub fn status(&self) -> StatusCode {
        match self {
            ApiErr::Invalid(_) => StatusCode::BAD_REQUEST,
            ApiErr::Unauthorized => StatusCode::UNAUTHORIZED,
            ApiErr::Busy(_) | ApiErr::RateLimited(_) => StatusCode::TOO_MANY_REQUESTS,
            ApiErr::Banned(_) | ApiErr::Challenge(_) => StatusCode::SERVICE_UNAVAILABLE,
            ApiErr::ModelNotFound(_) => StatusCode::NOT_FOUND,
            ApiErr::Upstream { .. } | ApiErr::Timeout | ApiErr::Transport(_) => {
                StatusCode::BAD_GATEWAY
            }
        }
    }

    /// 429/503 一律带 `retry-after`（§7 统一错误帧）。
    pub fn retry_after(&self) -> Option<u64> {
        match self {
            ApiErr::Busy(ra) | ApiErr::RateLimited(ra) | ApiErr::Banned(ra) => Some(*ra),
            ApiErr::Challenge(_) => Some(5),
            _ => None,
        }
    }

    /// 日志用错误分类码（成功为空）。
    pub fn code(&self) -> String {
        match self {
            ApiErr::Invalid(_) => "invalid_input".into(),
            ApiErr::Unauthorized => "unauthorized".into(),
            ApiErr::Busy(_) => "busy".into(),
            ApiErr::RateLimited(_) => "rate_limited".into(),
            ApiErr::Banned(_) => "banned".into(),
            ApiErr::Challenge(_) => "challenge_failed".into(),
            ApiErr::ModelNotFound(_) => "model_not_found".into(),
            ApiErr::Upstream { status, .. } => format!("upstream_{status}"),
            ApiErr::Timeout => "timeout".into(),
            ApiErr::Transport(_) => "transport".into(),
        }
    }

    /// 人类可读消息（中文，与原项目风格一致）。
    pub fn message(&self) -> String {
        match self {
            ApiErr::Invalid(m) => m.clone(),
            ApiErr::Unauthorized => "缺少或无效的 API Key".into(),
            ApiErr::Busy(_) => "并发请求已达上限，请稍后重试".into(),
            ApiErr::RateLimited(_) => "请求频率受限（429），请按 retry-after 退避后重试".into(),
            ApiErr::Banned(_) => {
                "当前出口 IP/浏览器指纹被上游临时限制（ERR_BN_LIMIT），该出口已进入冷却；请稍后重试或切换代理".into()
            }
            ApiErr::Challenge(_) => "上游交互式挑战处理失败，已重试上限次数；请稍后重试".into(),
            ApiErr::ModelNotFound(m) => format!("模型 {m} 不存在或未在上游注册"),
            ApiErr::Upstream { status, detail } => {
                format!("上游返回非预期响应（{status}）：{detail}")
            }
            ApiErr::Timeout => "上游请求超时".into(),
            ApiErr::Transport(m) => format!("上游连接失败：{m}"),
        }
    }

    /// OpenAI 系错误帧：`{"error":{message,type,code,param}}`。
    pub fn openai_body(&self) -> Value {
        let (r#type, code) = match self {
            ApiErr::Invalid(_) => ("invalid_request_error", "invalid_request"),
            ApiErr::Unauthorized => ("invalid_request_error", "invalid_api_key"),
            ApiErr::Busy(_) | ApiErr::RateLimited(_) => ("rate_limit_error", "rate_limit_exceeded"),
            ApiErr::Banned(_) => ("server_error", "upstream_banned"),
            ApiErr::Challenge(_) => ("server_error", "challenge_failed"),
            ApiErr::ModelNotFound(_) => ("invalid_request_error", "model_not_found"),
            ApiErr::Upstream { .. } => ("server_error", "upstream_error"),
            ApiErr::Timeout => ("server_error", "timeout"),
            ApiErr::Transport(_) => ("server_error", "transport_error"),
        };
        serde_json::json!({
            "error": {
                "message": self.message(),
                "type": r#type,
                "code": code,
                "param": Value::Null,
            }
        })
    }

    /// Anthropic 系错误帧：`{"type":"error","error":{type,message}}`。
    pub fn anthropic_body(&self) -> Value {
        let r#type = match self {
            ApiErr::Invalid(_) => "invalid_request_error",
            ApiErr::Unauthorized => "authentication_error",
            ApiErr::Busy(_) | ApiErr::RateLimited(_) => "rate_limit_error",
            ApiErr::Banned(_) | ApiErr::Challenge(_) | ApiErr::Upstream { .. } => "api_error",
            ApiErr::ModelNotFound(_) => "not_found_error",
            ApiErr::Timeout | ApiErr::Transport(_) => "api_error",
        };
        serde_json::json!({
            "type": "error",
            "error": { "type": r#type, "message": self.message() },
        })
    }

    /// 按协议族出错误帧。
    pub fn body_for(&self, proto: Proto) -> Value {
        match proto {
            Proto::Anthropic => self.anthropic_body(),
            Proto::OpenAi | Proto::Responses => self.openai_body(),
        }
    }
}
