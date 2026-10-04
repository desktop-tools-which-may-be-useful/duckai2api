//! 上游接入层（ARCHITECTURE §2.2 / §4 / §5 / §6）。
//!
//! - [`UpstreamClient`]：协议无关的接入 trait——API 层唯一的上游依赖面；
//! - `http` feature（默认）：纯 HTTP 适配器，reqwest + 本地 rquickjs 挑战求解；
//! - `browser` feature（可选）：chromiumoxide 驱动系统 Chrome 的高保真适配器；
//! - [`EgressPool`] / [`EgressMachine`]：代理池与封禁冷却状态机（§6，传输层真正生效）；
//! - `testutil` feature：[`MockUpstream`]——API 层契约测试的脚本化替身。
//!
//! 偏差记录（不改 ARCHITECTURE.md，仅在此注明）：`UpstreamRequest` 实体落在
//! duckai-types 而非本 crate §4 位置——api/webui/types 都需要该类型而依赖方向
//! 禁止反向引用本 crate，故此处只做 re-export。

mod cooldown;
mod pool;

#[cfg(feature = "browser")]
mod browser;
#[cfg(feature = "http")]
mod http;
#[cfg(feature = "testutil")]
mod testkit;

#[cfg(feature = "browser")]
pub use browser::{BrowserConfig, BrowserUpstream};
pub use cooldown::{
    BAN_CAP_SECS, COOLDOWN_CAP_SECS, EgressMachine, EgressState, EgressStatus, INITIAL_BAN_SECS,
    RATE_LIMIT_BASE_SECS,
};
#[cfg(feature = "http")]
pub use http::{HttpConfig, HttpUpstream};
pub use pool::{
    EgressHandle, EgressPool, PER_EGRESS_LIMIT, PoolError, parse_proxy_config, sanitize_proxy_url,
};
#[cfg(feature = "testutil")]
pub use testkit::{MockUpstream, ScriptedTurn};

pub use duckai_types::UpstreamRequest;

use std::sync::Arc;

use duckai_types::{ModelInfo, UpstreamError, UpstreamEvent};
use futures::stream::BoxStream;

/// `UpstreamClient::chat` 返回的事件流（协议中立，按上游真实节奏 yield）。
pub type UpstreamStream = BoxStream<'static, Result<UpstreamEvent, UpstreamError>>;

/// 一次接入调用的抽象：纯 HTTP 与浏览器模式共享同一 trait（§5.1 共享点）。
#[async_trait::async_trait]
pub trait UpstreamClient: Send + Sync {
    /// `"http" | "browser"`，仅供 /health 与观测展示。
    fn mode(&self) -> &'static str;

    /// 可用模型（上游拉取结果，30 分钟缓存；失败回落本地快照）。
    async fn list_models(&self) -> Result<Vec<ModelInfo>, UpstreamError>;

    /// 流式对话。挑战失效 / 418 / 429 的内部重试（§2.2 第 8 步）对调用方透明，
    /// 重试耗尽才向上抛分类错误。
    async fn chat(&self, req: UpstreamRequest) -> Result<UpstreamStream, UpstreamError>;

    /// 轻量探活（更新 x-fe-version / 代理健康），供 /health 与 WebUI 调用。
    async fn probe(&self) -> Result<(), UpstreamError>;

    /// `/health` 观测位：`(vqd_valid, fe_version_age_secs)`。
    /// 默认实现供非 http 适配器；http 适配器覆盖（§4 trait 的附加默认方法，不破坏实现面）。
    fn observability(&self) -> (bool, Option<u64>) {
        (false, None)
    }

    /// 运维入口：管理面经此读写出口状态机与代理列表（`admin_ban/unban`、
    /// 运行时增删代理）。适配器返回自己持有的池，保证与传输层同源；
    /// 无池的适配器（如 mock）返回 `None`，管理面按空出口表展示。
    fn egress_pool(&self) -> Option<Arc<EgressPool>> {
        None
    }
}

/// 接入模式（`DUCKAI_UPSTREAM`）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum UpstreamMode {
    /// 有 http 用 http；编译含 browser 且无 http 时降级 browser。
    #[default]
    Auto,
    Http,
    Browser,
}

impl UpstreamMode {
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "http" => Ok(Self::Http),
            "browser" => Ok(Self::Browser),
            other => Err(format!(
                "invalid DUCKAI_UPSTREAM {other:?}（期望 auto|http|browser）"
            )),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Http => "http",
            Self::Browser => "browser",
        }
    }
}

/// 工厂配置（duckai-server 从环境变量装配；此处不读环境，便于测试）。
#[derive(Debug, Clone)]
pub struct FactoryConfig {
    /// 上游 BASE（`DUCKAI_BASE`，默认 <https://duck.ai>）。
    pub base: String,
    pub mode: UpstreamMode,
    /// 运维捕获的 `x-vqd-hash-1`（`DUCKAI_VQD_OVERRIDE`，10 分钟 TTL）。
    pub vqd_override: Option<String>,
    /// `DUCKAI_NEW_CHAT`：true 时会话不粘。
    pub new_chat: bool,
    /// 代理列表（`DUCKAI_PROXIES` + `DUCKAI_PROXY` 合并后已切分；空=直连）。
    pub proxies: Vec<String>,
    /// `DUCKAI_CHROME_PATH`（browser 模式）。
    pub chrome_path: Option<String>,
}

impl Default for FactoryConfig {
    fn default() -> Self {
        Self {
            base: "https://duck.ai".to_string(),
            mode: UpstreamMode::Auto,
            vqd_override: None,
            new_chat: false,
            proxies: Vec::new(),
            chrome_path: None,
        }
    }
}

/// 按配置构造接入客户端（auto 的运行时降级事件写日志，并经 `mode()` 暴露）。
pub struct UpstreamFactory;

impl UpstreamFactory {
    pub fn build(cfg: FactoryConfig) -> Result<Arc<dyn UpstreamClient>, String> {
        let pool = Arc::new(EgressPool::new(&cfg.proxies).map_err(|e| e.to_string())?);
        match cfg.mode {
            UpstreamMode::Http => build_http(cfg, pool),
            UpstreamMode::Browser => build_browser(cfg, pool),
            UpstreamMode::Auto => {
                #[cfg(feature = "http")]
                {
                    build_http(cfg, pool)
                }
                #[cfg(not(feature = "http"))]
                {
                    build_browser(cfg, pool)
                }
            }
        }
    }
}

#[cfg(feature = "http")]
fn build_http(
    cfg: FactoryConfig,
    pool: Arc<EgressPool>,
) -> Result<Arc<dyn UpstreamClient>, String> {
    let upstream = HttpUpstream::new(cfg, pool)?;
    Ok(Arc::new(upstream))
}

#[cfg(not(feature = "http"))]
fn build_http(
    _cfg: FactoryConfig,
    _pool: Arc<EgressPool>,
) -> Result<Arc<dyn UpstreamClient>, String> {
    Err("http feature 未编译（duckai-upstream default-features 被关闭）".to_string())
}

#[cfg(feature = "browser")]
fn build_browser(
    cfg: FactoryConfig,
    pool: Arc<EgressPool>,
) -> Result<Arc<dyn UpstreamClient>, String> {
    let upstream = BrowserUpstream::new(cfg, pool)?;
    Ok(Arc::new(upstream))
}

#[cfg(not(feature = "browser"))]
fn build_browser(
    _cfg: FactoryConfig,
    _pool: Arc<EgressPool>,
) -> Result<Arc<dyn UpstreamClient>, String> {
    Err("browser feature 未编译（需 `--features browser` 重新构建）".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_parse_roundtrip() {
        assert_eq!(UpstreamMode::parse("auto").unwrap(), UpstreamMode::Auto);
        assert_eq!(UpstreamMode::parse(" HTTP ").unwrap(), UpstreamMode::Http);
        assert_eq!(
            UpstreamMode::parse("Browser").unwrap(),
            UpstreamMode::Browser
        );
        assert!(UpstreamMode::parse("ftp").is_err());
    }

    #[test]
    fn factory_builds_default_auto_http() {
        let client = UpstreamFactory::build(FactoryConfig::default()).expect("auto → http");
        assert_eq!(client.mode(), "http");
    }
}
