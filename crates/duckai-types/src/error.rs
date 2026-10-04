use thiserror::Error;

/// 出口（egress）标识——对外错误只暴露脱敏标签，不携带代理凭据。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EgressScope {
    Direct,
    Proxy(usize),
}

impl std::fmt::Display for EgressScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EgressScope::Direct => write!(f, "direct"),
            EgressScope::Proxy(i) => write!(f, "proxy#{i}"),
        }
    }
}

/// 上游错误分类（决定 API 层响应码与错误帧）。
///
/// 措辞纪律：上游是匿名 IP/指纹受限，任何文案禁止出现「账号被封」一类表述。
#[derive(Debug, Error)]
pub enum UpstreamError {
    /// 429 / 全局并发闸门打满 → 429 + retry-after。
    #[error("upstream rate limited, retry after {retry_after}s")]
    RateLimited { retry_after: u64 },

    /// 418 ERR_BN_LIMIT 等出口受限 → 503 + retry-after（IP/指纹受限，非账号问题）。
    #[error("egress {scope} is restricted by upstream (IP/fingerprint limited)")]
    Banned { scope: EgressScope },

    /// 挑战求解退化链耗尽（本地求解 N 次失败 + 无 override）→ 503。
    #[error("challenge solving failed after exhausting fallbacks")]
    ChallengeFailed,

    /// 未知模型 → 404（本地目录校验，P1-8 修正位）。
    #[error("unknown model: {0}")]
    ModelNotFound(String),

    /// 上游 4xx/5xx（非 418/429/挑战）→ 502。
    #[error("upstream responded {status}: {body}")]
    Upstream { status: u16, body: String },

    /// 请求本身不合法 → 400。
    #[error("invalid input: {0}")]
    InvalidInput(String),

    /// 上游超时（默认 300s）→ 502。
    #[error("upstream timed out")]
    Timeout,

    /// 传输层故障 → 502。
    #[error("transport error: {0}")]
    Transport(String),
}

impl UpstreamError {
    /// 供 /health 与日志的短分类码。
    pub fn kind(&self) -> &'static str {
        match self {
            UpstreamError::RateLimited { .. } => "rate_limited",
            UpstreamError::Banned { .. } => "banned",
            UpstreamError::ChallengeFailed => "challenge_failed",
            UpstreamError::ModelNotFound(_) => "model_not_found",
            UpstreamError::Upstream { .. } => "upstream",
            UpstreamError::InvalidInput(_) => "invalid_input",
            UpstreamError::Timeout => "timeout",
            UpstreamError::Transport(_) => "transport",
        }
    }
}
