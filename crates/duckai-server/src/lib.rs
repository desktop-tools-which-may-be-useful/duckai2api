//! duckai2api 装配层（§3.6）：配置 → 上游工厂 → API/WebUI 路由拼装。
//!
//! 分层：本 crate 只消费 `duckai-api` 的 `ApiState`/`router` 与 `duckai-webui`
//! 的 `router`（经 `duckai-types` 的 `AdminControl`/`AdminPassword` trait），
//! 不反向依赖 UI 内部。持久化走 `duckai-store`（sqlite：密钥/口令/设置，配置类数据）。

mod config;
mod password;

pub use config::{ServerConfig, load_dotenv};
pub use password::ServerPassword;

use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};

use duckai_store::Store;
use duckai_types::model::ModelCatalog;
use duckai_types::{
    AdminControl, AdminPassword, AdminState, EgressHealth, HealthSnapshot, ModelInfo,
    SettingsSnapshot, UpstreamHealth,
};
use duckai_upstream::EgressPool;

/// 管理面实现：API 状态（闸门/默认模型/日志环）+ 启动配置快照 + 出口池 + 存储写穿。
///
/// 关键约束：所有"配置项"都是**活的**——闸门上限经 `set_max_concurrency`
/// 立即生效、默认模型经写锁实时生效、代理经 `pool.reconfigure` 在途安全变更，
/// 管理面读写同一对象（不存在只展示不生效的死配置）。凡是库里持久化的项，
/// 写穿顺序 = **先落库成功再改内存**（重启与运行态永远一致）。
pub struct ServerAdmin {
    state: duckai_api::ApiState,
    bind: String,
    new_chat: bool,
    /// 口令契约（只读来源：WebUI 登录与 `admin_password_set` 快照共用）。
    password: Arc<dyn AdminPassword>,
    /// 配置存储（密钥/口令/设置写穿点）。
    store: Arc<Store>,
    /// 模型列表缓存（30 分钟后台刷新；空时回落本地目录快照）。
    models: RwLock<Vec<ModelInfo>>,
}

impl ServerAdmin {
    pub fn new(
        state: duckai_api::ApiState,
        bind: String,
        new_chat: bool,
        password: Arc<dyn AdminPassword>,
        store: Arc<Store>,
    ) -> Arc<Self> {
        Arc::new(Self {
            state,
            bind,
            new_chat,
            password,
            store,
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
            auth_enabled: self.state.auth.enabled(),
            admin_password_set: self.password.available(),
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
        // 先落库（库失败则不改内存，避免「看着改了重启又回退」）。
        self.store
            .set_setting("default_model", m)
            .map_err(|e| e.to_string())?;
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
        self.store
            .set_setting("max_concurrency", &n.to_string())
            .map_err(|e| e.to_string())?;
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

/// 按配置装配完整应用：打开/引导 sqlite 库 → fail-fast 绑定校验 → 工厂构建上游
/// 客户端 → API 路由（Bearer 鉴权层）→ `/health`（无鉴权）→ WebUI（feature `webui`）
/// → 模型缓存后台刷新。
pub fn build(cfg: ServerConfig) -> Result<App, String> {
    build_with_store(cfg, None)
}

/// 同 [`build`]，但允许注入已打开的库（测试：共享句柄断言引导结果；生产传 `None`）。
///
/// 引导语义（「库为准」）：
/// 1. `settings` 表为空 → 把 env 派生配置写入（一次性 bootstrap）；之后读库覆盖 env；
/// 2. `api_keys` 表为空且 env 有 `DUCKAI_DEFAULT_API_KEY` → 导入第一把 key；
/// 3. 非回环绑定 + 无任何活跃 key → 拒绝启动（fail-fast 移到库引导之后）。
pub fn build_with_store(cfg: ServerConfig, store: Option<Arc<Store>>) -> Result<App, String> {
    let store = match store {
        Some(s) => s,
        None => Arc::new(Store::open(&cfg.db_path).map_err(|e| e.to_string())?),
    };

    let cfg = bootstrap_settings(&store, cfg)?;
    if let Some(key) = cfg.default_api_key.as_deref() {
        store
            .import_default_key(key, "DUCKAI_DEFAULT_API_KEY 引导")
            .map_err(|e| e.to_string())?;
    }
    cfg.validate_bind(store.has_active_key())?;

    let password: Arc<dyn AdminPassword> = Arc::new(ServerPassword::new(
        store.clone(),
        cfg.default_admin_password.clone(),
    ));

    let client = duckai_upstream::UpstreamFactory::build(cfg.factory()?)?;
    let state = duckai_api::ApiState::with_auth(
        client,
        store.clone(),
        cfg.default_model.clone(),
        cfg.max_concurrency,
    );
    let admin = ServerAdmin::new(
        state.clone(),
        cfg.addr(),
        cfg.new_chat,
        password.clone(),
        store,
    );
    spawn_model_refresh(admin.clone());

    let api = duckai_api::router(state.clone());
    let health_route = Router::new()
        .route("/health", get(health))
        .with_state(admin.clone());
    let router = api.merge(health_route);

    // webui feature 关闭时不注册静态与管理路由（#[cfg] 避免无条件 mut）
    #[cfg(feature = "webui")]
    let router = router.merge(duckai_webui::router(admin.clone(), password));

    Ok(App {
        router,
        addr: cfg.addr(),
        admin,
        state,
    })
}

/// 设置表的首次引导与「库覆盖 env」：
/// - 空表时写入 env 派生值（base/default_model/vqd_override/new_chat/max_concurrency）；
/// - 随后读库回填 cfg（库里有值以库为准；库值非法 → 拒绝启动）。
fn bootstrap_settings(store: &Store, cfg: ServerConfig) -> Result<ServerConfig, String> {
    let seed: [(&str, String); 5] = [
        ("base", cfg.base.clone()),
        ("default_model", cfg.default_model.clone()),
        ("vqd_override", cfg.vqd_override.clone().unwrap_or_default()),
        (
            "new_chat",
            if cfg.new_chat { "true" } else { "false" }.to_string(),
        ),
        ("max_concurrency", cfg.max_concurrency.to_string()),
    ];
    for (k, v) in seed {
        if store.get_setting(k).is_none() {
            store.set_setting(k, &v).map_err(|e| e.to_string())?;
        }
    }

    let mut cfg = cfg;
    if let Some(v) = store.get_setting("base") {
        let trimmed = v.trim().trim_end_matches('/');
        if trimmed.is_empty() {
            return Err("库内 base 设置为空（settings 表已污染，删除该行修复）".to_string());
        }
        cfg.base = trimmed.to_string();
    }
    if let Some(v) = store.get_setting("default_model") {
        if v.trim().is_empty() {
            return Err("库内 default_model 设置为空".to_string());
        }
        cfg.default_model = v;
    }
    if let Some(v) = store.get_setting("vqd_override") {
        cfg.vqd_override = (!v.trim().is_empty()).then_some(v);
    }
    if let Some(v) = store.get_setting("new_chat") {
        cfg.new_chat = matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "true" | "1" | "yes" | "on"
        );
    }
    if let Some(v) = store.get_setting("max_concurrency") {
        cfg.max_concurrency = v
            .trim()
            .parse::<usize>()
            .ok()
            .filter(|n| *n >= 1)
            .ok_or_else(|| format!("库内 max_concurrency 非法：{v:?}"))?;
    }
    Ok(cfg)
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
            "DUCKAI_DEFAULT_API_KEY" => Some("sk-e2e".to_string()),
            "DUCKAI_DB_PATH" => Some(":memory:".to_string()),
            _ => None,
        })
        .expect("test config")
    }

    #[test]
    fn build_assembles_all_layers() {
        let app = build(cfg_for("https://duck.ai")).expect("build");
        assert_eq!(app.addr, "127.0.0.1:8080");
        assert_eq!(app.admin.settings().bind, "127.0.0.1:8080");
        assert!(
            app.state.auth.enabled() && app.state.auth.validate("sk-e2e"),
            "引导 key 已入库并接入鉴权"
        );
        assert!(!app.state.auth.validate("sk-wrong"), "库内只认真实 key");
        assert!(app.admin.settings().auth_enabled, "快照显示鉴权已开");
    }

    /// 空库引导：env 默认值进 settings/api_keys；库非空后 env 不再覆盖。
    #[test]
    fn bootstrap_seeds_settings_and_key_then_db_wins() {
        let store = Arc::new(Store::open_in_memory().expect("内存库"));
        build_with_store(cfg_for("https://duck.ai"), Some(store.clone())).expect("首启引导");
        assert_eq!(
            store.get_setting("base").as_deref(),
            Some("https://duck.ai"),
            "env → settings 首次写入"
        );
        assert!(store.has_active_key(), "默认 key 已导入为第一把");

        // 库里写入的设置优先于 env（重启后库为准）
        store
            .set_setting("default_model", "db-model")
            .expect("写库");
        store.set_setting("new_chat", "true").expect("写库");
        let app2 = build_with_store(cfg_for("https://duck.ai"), Some(store.clone())).expect("二启");
        assert_eq!(app2.state.default_model(), "db-model", "库值覆盖 env");
        assert!(app2.admin.settings().new_chat, "布尔库值生效");

        // 库非空后 env 默认 key 不再被导入（store 单测覆盖幂等性；
        // 这里验证已有 key 与新增 key 都能过鉴权）
        let second = store.create_key("网关A").expect("建 key");
        let app3 = build_with_store(cfg_for("https://duck.ai"), Some(store.clone())).expect("三启");
        assert!(app3.state.auth.validate("sk-e2e"), "引导 key 仍有效");
        assert!(app3.state.auth.validate(&second), "手建 key 同样有效");
    }

    /// fail-fast 移位：非回环 + 库内外皆无 key → build 拒绝。
    #[test]
    fn non_loopback_keyless_build_refused() {
        let cfg = ServerConfig::from_lookup(&|k: &str| match k {
            "DUCKAI_BIND" => Some("0.0.0.0".to_string()),
            "DUCKAI_DB_PATH" => Some(":memory:".to_string()),
            _ => None,
        })
        .expect("解析");
        let err = match build(cfg) {
            Err(e) => e,
            Ok(_) => panic!("非回环无 key 必须拒绝启动"),
        };
        assert!(err.contains("DUCKAI_DEFAULT_API_KEY"), "提示引导键：{err}");
    }

    /// 管理面写穿：先落库、后改内存；重启后库值依然在。
    #[tokio::test]
    async fn admin_settings_write_through_store() {
        let store = Arc::new(Store::open_in_memory().expect("内存库"));
        let app = build_with_store(cfg_for("https://duck.ai"), Some(store.clone())).expect("build");
        let admin = app.admin.clone();

        admin
            .set_default_model("persisted-model".into())
            .expect("set");
        admin.set_max_concurrency(5).expect("set max");
        assert_eq!(
            store.get_setting("default_model").as_deref(),
            Some("persisted-model")
        );
        assert_eq!(store.get_setting("max_concurrency").as_deref(), Some("5"));

        // 重启（同库）后从库里读回
        let app2 = build_with_store(cfg_for("https://duck.ai"), Some(store.clone())).expect("重启");
        assert_eq!(app2.state.default_model(), "persisted-model");
        assert_eq!(app2.state.gate.max(), 5);
        // 拒绝非法值不改库
        assert!(admin.set_max_concurrency(0).is_err());
        assert_eq!(store.get_setting("max_concurrency").as_deref(), Some("5"));
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
