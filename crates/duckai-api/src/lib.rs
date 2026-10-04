//! API 层（ARCHITECTURE §4）：三协议端点 + Bearer 鉴权 + 全局并发闸门。
//!
//! 分层约束：本 crate 只依赖 [`duckai_upstream::UpstreamClient`] trait 与
//! duckai-types / duckai-protocol；绝不触碰上游适配器内部类型，也绝不让
//! 协议层（duckai-protocol）反向依赖 axum。

mod error;
mod gate;
mod logs;
mod prep;
mod resp;
mod stream;

pub mod anthropic;
pub mod openai;
pub mod responses;

use std::sync::Arc;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::header::AUTHORIZATION;
use axum::middleware::Next;
use axum::response::Response;
use axum::routing::{get, post};
use duckai_types::model::ModelCatalog;
use duckai_upstream::UpstreamClient;
use serde_json::json;

pub use crate::error::{ApiErr, Proto};
pub use crate::gate::{ConcurrencyGate, GatePermit};
pub use crate::logs::{LogRing, now_ms, now_secs};
pub use crate::prep::{Prepared, prepare};
pub use crate::resp::{error_response, json_response, sse_response};
pub use crate::stream::{TextGate, completion_id, hex24};

/// API 层共享状态（Clone 只克隆 Arc）。
#[derive(Clone)]
pub struct ApiState {
    /// 上游接入（http/browser/testutil 适配器同一 trait）。
    pub upstream: Arc<dyn UpstreamClient>,
    /// `DUCKAI_API_KEY`；`None` = 关闭鉴权（仅回环地址允许，由 server 启动期强制）。
    pub api_key: Option<String>,
    pub gate: Arc<ConcurrencyGate>,
    /// `DUCKAI_MODEL`：请求缺省模型。
    /// 默认模型：`DUCKAI_MODEL`，管理面 `set_default_model` 运行时可改
    /// （内部读写锁，所有协议请求实时读到新值）。
    pub default_model: std::sync::Arc<std::sync::RwLock<String>>,
    /// 结构化请求日志（server 装配，WebUI 只读消费）。
    pub logs: Arc<LogRing>,
}

impl ApiState {
    pub fn new(
        upstream: Arc<dyn UpstreamClient>,
        api_key: Option<String>,
        default_model: String,
        max_concurrency: usize,
    ) -> Self {
        Self {
            upstream,
            api_key,
            gate: Arc::new(ConcurrencyGate::new(max_concurrency)),
            default_model: std::sync::Arc::new(std::sync::RwLock::new(default_model)),
            logs: Arc::new(LogRing::default()),
        }
    }

    /// 当前默认模型快照（读锁；中毒取内值，管理面写入不会锁死后续请求）。
    pub fn default_model(&self) -> String {
        self.default_model
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// 恒定时间比较（避免字符串短路时序）。
fn bearer_matches(expected: &str, presented: &str) -> bool {
    let a = expected.as_bytes();
    let b = presented.as_bytes();
    let mut diff = (a.len() ^ b.len()) as u8;
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= x ^ y;
    }
    diff == 0
}

/// `/v1/*` Bearer 鉴权中间件：401 错误帧按路径分族（`/v1/messages` 用 Anthropic 形状）。
pub async fn auth_middleware(State(state): State<ApiState>, req: Request, next: Next) -> Response {
    let Some(expected) = state.api_key.as_deref().filter(|k| !k.is_empty()) else {
        return next.run(req).await;
    };
    let presented = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer ").map(str::trim));
    let ok = presented.is_some_and(|token| bearer_matches(expected, token));
    if ok {
        return next.run(req).await;
    }
    let proto = if req.uri().path().ends_with("/messages") {
        Proto::Anthropic
    } else {
        Proto::OpenAi
    };
    error_response(proto, &ApiErr::Unauthorized)
}

/// 装配 `/v1` 子路由（鉴权层 + 三协议 + 模型列表）。
pub fn router(state: ApiState) -> Router {
    Router::new()
        .route("/v1/chat/completions", post(openai::chat))
        .route("/v1/messages", post(anthropic::messages))
        .route("/v1/responses", post(responses::create))
        .route("/v1/models", get(models_list))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state)
}

/// `GET /v1/models`：上游拉取（30min 缓存，P1-8 快照兜底）+ 别名。
async fn models_list(State(state): State<ApiState>) -> Response {
    let models = match state.upstream.list_models().await {
        Ok(v) if !v.is_empty() => v,
        _ => ModelCatalog::snapshot().list().to_vec(),
    };
    let data: Vec<_> = models
        .iter()
        .map(|m| {
            json!({
                "id": m.id,
                "object": "model",
                "created": 0,
                "owned_by": m.owned_by,
                "aliases": m.aliases,
            })
        })
        .collect();
    json_response(
        axum::http::StatusCode::OK,
        json!({ "object": "list", "data": data }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_matches_exact() {
        assert!(bearer_matches("sk-abc", "sk-abc"));
        assert!(!bearer_matches("sk-abc", "sk-abd"));
        assert!(!bearer_matches("sk-abc", "sk-ab"));
        assert!(!bearer_matches("sk-abc", "sk-abcd"));
        assert!(!bearer_matches("sk-abc", ""));
    }

    #[test]
    fn gate_counts_inflight() {
        let gate = ConcurrencyGate::new(2);
        let p1 = gate.try_acquire().expect("first permit");
        let p2 = gate.try_acquire().expect("second permit");
        assert!(gate.try_acquire().is_none(), "耗尽后必须非阻塞失败");
        assert_eq!(gate.inflight(), 2);
        drop(p1);
        assert_eq!(gate.inflight(), 1, "归还一个令牌 → 在途 1");
        let p3 = gate.try_acquire().expect("归还后可再次获取");
        assert_eq!(gate.inflight(), 2);
        drop(p3);
        drop(p2);
        assert_eq!(gate.inflight(), 0, "全部归还 → 在途 0");
    }
}
