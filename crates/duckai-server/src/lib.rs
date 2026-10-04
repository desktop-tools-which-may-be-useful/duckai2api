//! duckai2api 装配层（§3.6）：配置 → 上游工厂 → API/WebUI 路由拼装。
//!
//! 分层：本 crate 只消费 `duckai-api` 的 `ApiState`/`router` 与 `duckai-webui`
//! 的 `router`（经 `duckai-types` 的 `AdminControl` trait），不反向依赖 UI 内部。

mod config;

pub use config::{ServerConfig, load_dotenv};

use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};

use duckai_types::model::ModelCatalog;
use duckai_types::{
    AdminControl, AdminState, EgressHealth, HealthSnapshot, ModelInfo, SettingsSnapshot,
    UpstreamHealth,
};
use duckai_upstream::EgressPool;

/// 管理面实现：API 状态（闸门/默认模型/日志环）+ 启动配置快照 + 出口池。
///
/// 关键约束：所有"配置项"都是**活的**——闸门上限经 `set_max_concurrency`
/// 立即生效、默认模型经写锁实时生效、代理经 `pool.reconfigure` 在途安全变更，
/// 管理面读写同一对象（不存在只展示不生效的死配置）。
pub struct ServerAdmin {
    state: duckai_api::ApiState,
    bind: String,
    new_chat: bool,
    admin_password_set: bool,
    /// 模型列表缓存（30 分钟后台刷新；空时回落本地目录快照）。
    models: RwLock<Vec<ModelInfo>>,
}

impl ServerAdmin {
    pub fn new(
        state: duckai_api::ApiState,
        bind: String,
        new_chat: bool,
        admin_password_set: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            state,
            bind,
            new_chat,
            admin_password_set,
            models: RwLock::new(Vec::new()),
        })
    }

    fn pool(&self) -> Option<Arc<EgressPool>> {
        self.state.upstream.egress_pool()
    }

    fn pool_ref(&self) -> Result<Arc<EgressPool>, String> {
        self.pool()
            .ok_or_else(|| "当前适配器未持有出口池，无法在线变更出口".to_string())
    }

    fn read_models(&self) -> Vec<ModelInfo> {
        let cached = self
            .models
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if cached.is_empty() {
            ModelCatalog::snapshot().list().to_vec()
        } else {
            cached
        }
    }
}

impl AdminState for ServerAdmin {
    fn health(&self) -> HealthSnapshot {
        let (vqd_valid, fe_version_age) = self.state.upstream.observability();
        let (healthy, total, banned) = self.pool().map_or((0, 0, 0), |p| {
            (p.healthy_count(), p.len(), p.banned_count())
        });
        HealthSnapshot {
            status: "ok".to_string(),
            upstream: UpstreamHealth {
                mode: self.state.upstream.mode().to_string(),
                vqd_valid,
                fe_version_age,
            },
            egress: EgressHealth {
                healthy,
                total,
                banned,
            },
            inflight: self.state.gate.inflight(),
        }
    }

    fn egresses(&self) -> Vec<duckai_types::EgressSnapshot> {
        self.pool().map_or_else(Vec::new, |p| p.snapshot())
    }

    fn logs(&self, lines: usize) -> Vec<duckai_types::LogEntry> {
        self.state.logs.recent(lines)
    }

    fn settings(&self) -> SettingsSnapshot {
        let proxies = self.pool().map_or_else(Vec::new, |p| {
            // 池快照已脱敏（仅协议+主机，无凭据）。
            p.snapshot().into_iter().filter_map(|e| e.proxy).collect()
        });
        SettingsSnapshot {
            default_model: self.state.default_model(),
            max_concurrency: self.state.gate.max(),
            bind: self.bind.clone(),
            auth_enabled: self.state.api_key.as_deref().is_some_and(|k| !k.is_empty()),
            admin_password_set: self.admin_password_set,
            upstream_mode: self.state.upstream.mode().to_string(),
            new_chat: self.new_chat,
            proxies,
        }
    }

    fn models(&self) -> Vec<ModelInfo> {
        self.read_models()
    }
}

impl AdminControl for ServerAdmin {
    fn ban_egress(&self, index: usize) -> Result<(), String> {
        self.pool_ref()?.admin_ban(index)
    }

    fn unban_egress(&self, index: usize) -> Result<(), String> {
        self.pool_ref()?.admin_unban(index)
    }

    /// 探测是网络操作：管理入口保持同步 trait，落到这里 spawn。
    fn trigger_probe(&self) {
        let client = self.state.upstream.clone();
        tokio::spawn(async move {
            if let Err(e) = client.probe().await {
                tracing::debug!("手动探测失败：{e}");
            }
        });
    }

    fn add_proxy(&self, url: String) -> Result<(), String> {
        let pool = self.pool_ref()?;
        let mut urls = pool.proxy_urls();
        let norm = duckai_upstream::sanitize_proxy_url(&url);
        if urls
            .iter()
            .any(|u| duckai_upstream::sanitize_proxy_url(u) == norm)
        {
            return Err("代理已存在".to_string());
        }
        urls.push(url);
        pool.reconfigure(&urls)
    }

    fn remove_proxy(&self, url: String) -> Result<(), String> {
        let pool = self.pool_ref()?;
        let mut urls = pool.proxy_urls();
        let norm = duckai_upstream::sanitize_proxy_url(&url);
        let before = urls.len();
        urls.retain(|u| duckai_upstream::sanitize_proxy_url(u) != norm && *u != url);
        if urls.len() == before {
            return Err("代理不存在".to_string());
        }
        pool.reconfigure(&urls)
    }

    fn set_default_model(&self, model: String) -> Result<(), String> {
        let m = model.trim();
        if m.is_empty() {
            return Err("模型名不能为空".to_string());
        }
        *self
            .state
            .default_model
            .write()
            .unwrap_or_else(|e| e.into_inner()) = m.to_string();
        Ok(())
    }

    fn set_max_concurrency(&self, n: usize) -> Result<(), String> {
        if n == 0 {
            return Err("并发上限必须 ≥1".to_string());
        }
        self.state.gate.set_max(n);
        Ok(())
    }
}

/// 组装完成的应用：路由 + 监听地址（供 main 与集成测试共用）。
pub struct App {
    pub router: Router,
    pub addr: String,
    pub admin: Arc<ServerAdmin>,
    pub state: duckai_api::ApiState,
}

/// `GET /health`（免鉴权；§3.2/S2 契约）。
async fn health(State(admin): State<Arc<ServerAdmin>>) -> Json<duckai_types::HealthSnapshot> {
    Json(AdminState::health(admin.as_ref()))
}

/// 按配置装配完整应用：工厂构建上游客户端 → API 路由（Bearer 鉴权层）→
/// `/health`（无鉴权）→ WebUI（feature `webui`）→ 模型缓存后台刷新。
pub fn build(cfg: ServerConfig) -> Result<App, String> {
    let client = duckai_upstream::UpstreamFactory::build(cfg.factory()?)?;
    let state = duckai_api::ApiState::new(
        client,
        cfg.api_key.clone(),
        cfg.default_model.clone(),
        cfg.max_concurrency,
    );
    let admin = ServerAdmin::new(
        state.clone(),
        cfg.addr(),
        cfg.new_chat,
        cfg.admin_password.is_some(),
    );
    spawn_model_refresh(admin.clone());

    let api = duckai_api::router(state.clone());
    let health_route = Router::new()
        .route("/health", get(health))
        .with_state(admin.clone());
    let router = api.merge(health_route);

    // webui feature 关闭时不注册静态与管理路由（#[cfg] 避免无条件 mut）
    #[cfg(feature = "webui")]
    let router = router.merge(duckai_webui::router(
        admin.clone(),
        cfg.admin_password.clone(),
    ));

    Ok(App {
        router,
        addr: cfg.addr(),
        admin,
        state,
    })
}

/// 模型列表后台刷新：启动即拉一次，成功 30 分钟、失败 5 分钟重试。
/// 失败/未就绪期间管理面回落本地目录快照，绝不空列表。
fn spawn_model_refresh(admin: Arc<ServerAdmin>) {
    if tokio::runtime::Handle::try_current().is_err() {
        return; // 非异步上下文（防御）：跳过，管理面用快照兜底
    }
    tokio::spawn(async move {
        loop {
            let ok = match admin.state.upstream.list_models().await {
                Ok(v) if !v.is_empty() => {
                    *admin.models.write().unwrap_or_else(|e| e.into_inner()) = v;
                    true
                }
                _ => false,
            };
            tokio::time::sleep(Duration::from_secs(if ok { 1800 } else { 300 })).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ServerConfig;

    fn cfg_for(base: &str) -> ServerConfig {
        ServerConfig::from_lookup(&|k: &str| match k {
            "DUCKAI_BASE" => Some(base.to_string()),
            "DUCKAI_API_KEY" => Some("sk-e2e".to_string()),
            _ => None,
        })
        .expect("test config")
    }

    #[test]
    fn build_assembles_all_layers() {
        let app = build(cfg_for("https://duck.ai")).expect("build");
        assert_eq!(app.addr, "127.0.0.1:8080");
        assert_eq!(app.admin.settings().bind, "127.0.0.1:8080");
        assert!(app.state.api_key.as_deref() == Some("sk-e2e"), "key 已注入");
    }

    /// 管理面写操作必须落到真实对象（反死配置断言）。
    #[tokio::test]
    async fn admin_controls_are_live() {
        let app = build(cfg_for("https://duck.ai")).expect("build");
        let admin = app.admin.clone();

        // 默认模型：写锁实时可见
        assert_eq!(admin.settings().default_model, "gpt-5.6-luna");
        admin.set_default_model("new-model".into()).expect("set");
        assert_eq!(admin.settings().default_model, "new-model");
        assert_eq!(app.state.default_model(), "new-model");
        assert!(admin.set_default_model("  ".into()).is_err());

        // 并发上限：闸门立即生效
        let before = admin.settings().max_concurrency;
        assert_eq!(before, 8);
        admin.set_max_concurrency(3).expect("set max");
        assert_eq!(admin.settings().max_concurrency, 3);
        assert_eq!(app.state.gate.max(), 3);
        assert!(admin.set_max_concurrency(0).is_err());

        // 出口池：单出口直连（HTTP 适配器始终有池）
        let eg = admin.egresses();
        assert_eq!(eg.len(), 1, "默认单出口直连");
        assert_eq!(eg[0].label, "direct");

        // 手动封禁/解封（P1-6：banned 可恢复）
        admin.ban_egress(0).expect("ban");
        assert_eq!(admin.egresses()[0].state, "Banned");
        assert!(admin.ban_egress(99).is_err(), "越界拒绝");
        admin.unban_egress(0).expect("unban");
        assert_eq!(admin.egresses()[0].state, "Healthy");

        // 代理在线增删（reconfigure 即刻反映到 settings）
        admin
            .add_proxy("http://user:secret@10.0.0.1:8080".into())
            .expect("add");
        let st = admin.settings();
        assert_eq!(st.proxies.len(), 1);
        assert!(
            !st.proxies[0].contains("secret"),
            "管理面绝不回显凭据：{}",
            st.proxies[0]
        );
        assert!(
            admin.add_proxy("http://10.0.0.1:8080".into()).is_err(),
            "同主机去重"
        );
        admin
            .remove_proxy("http://user:secret@10.0.0.1:8080".into())
            .expect("remove");
        assert!(admin.settings().proxies.is_empty());
        assert!(
            admin.remove_proxy("http://x:1".into()).is_err(),
            "不存在拒绝"
        );

        // 探测 spawn（同步 trait；无 runtime 任务则管理面仍可用）
        admin.trigger_probe();

        // 健康快照字段来自上游实况
        let h = admin.health();
        assert_eq!(h.status, "ok");
        assert_eq!(h.upstream.mode, "http");
        assert_eq!(h.egress.total, 1);
    }
}
