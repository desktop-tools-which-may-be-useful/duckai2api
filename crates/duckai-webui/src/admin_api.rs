//! 管理 API：cookie 会话鉴权 + AdminState/AdminControl 的 HTTP 暴露。
//!
//! 权限模型（口令来源由装配层注入的 `AdminPassword` trait 决定）：
//! - 已配置口令（库内自定义或 `DUCKAI_DEFAULT_ADMIN_PASSWORD`）：读写均需
//!   `duckai_admin` 会话 cookie（POST /login 换取，12h 有效）；
//! - 未配置任何口令：WebUI 只读，且仅回环来源可访问；一切写操作恒 403。

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Path, Query, Request, State};
use axum::http::{Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use duckai_types::{AdminControl, AdminPassword, EgressPolicy};

const COOKIE_NAME: &str = "duckai_admin";
const SESSION_TTL: Duration = Duration::from_secs(12 * 3600);

/// WebUI 全局状态（与 `AdminControl` 组合；trait 本体实现在 duckai-server）。
pub struct UiState {
    pub admin: Arc<dyn AdminControl>,
    /// 口令契约（库内 argon2id 自定义口令 / env 默认口令回落，实现在装配层）。
    password: Arc<dyn AdminPassword>,
    sessions: Mutex<HashMap<String, Instant>>,
}

impl UiState {
    pub fn new(admin: Arc<dyn AdminControl>, password: Arc<dyn AdminPassword>) -> Self {
        Self {
            admin,
            password,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// 无口令 = 只读模式。
    pub fn read_only(&self) -> bool {
        !self.password.available()
    }

    fn issue_token(&self) -> String {
        let raw = rand::random::<[u8; 16]>();
        raw.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn session_valid(&self, token: &str) -> bool {
        if token.is_empty() {
            return false;
        }
        let mut guard = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        match guard.get(token) {
            Some(exp) if *exp > Instant::now() => true,
            Some(_) => {
                guard.remove(token);
                false
            }
            None => false,
        }
    }

    fn open_session(&self, token: String) {
        let mut g = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        g.retain(|_, exp| *exp > Instant::now());
        g.insert(token, Instant::now() + SESSION_TTL);
    }

    fn close_session(&self, token: &str) {
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(token);
    }

    fn token_from(req: &Request) -> Option<String> {
        req.headers()
            .get(header::COOKIE)?
            .to_str()
            .ok()?
            .split(';')
            .filter_map(|pair| {
                let (k, v) = pair.trim().split_once('=')?;
                (k == COOKIE_NAME).then(|| v.to_string())
            })
            .next()
    }
}

fn err(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}

fn is_loopback(ip: IpAddr) -> bool {
    ip.is_loopback()
}

pub(crate) async fn auth_guard(
    State(st): State<Arc<UiState>>,
    req: Request,
    next: Next,
) -> Response {
    let path = req.uri().path().to_string();
    // 登录端点豁免（本身执行口令校验）。
    if path.ends_with("/login") {
        return next.run(req).await;
    }

    let remote = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip());
    let loopback_ok = remote.is_none_or(is_loopback);

    if st.read_only() {
        // 无口令：仅回环可访问；写操作恒 403。
        if !loopback_ok {
            return err(
                StatusCode::FORBIDDEN,
                "未配置管理口令（库内或 DUCKAI_DEFAULT_ADMIN_PASSWORD），管理面仅限本机访问",
            );
        }
        let is_write = !matches!(req.method(), &Method::GET | &Method::HEAD);
        if is_write {
            return err(
                StatusCode::FORBIDDEN,
                "只读模式：未配置管理口令，写操作被拒绝",
            );
        }
    } else {
        // 有口令：读写都要求有效会话。
        let ok = UiState::token_from(&req).is_some_and(|t| st.session_valid(&t));
        if !ok {
            return err(StatusCode::UNAUTHORIZED, "未登录或会话已过期");
        }
    }
    next.run(req).await
}

// ----------------------------------------------------------------- 认证

#[derive(Deserialize)]
struct LoginBody {
    password: String,
}

async fn login(State(st): State<Arc<UiState>>, Json(body): Json<LoginBody>) -> Response {
    if !st.password.available() {
        return err(StatusCode::FORBIDDEN, "未配置管理口令，无需登录");
    }
    if !st.password.verify(&body.password) {
        return err(StatusCode::UNAUTHORIZED, "口令不正确");
    }
    let token = st.issue_token();
    st.open_session(token.clone());
    let cookie = format!(
        "{COOKIE_NAME}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}",
        SESSION_TTL.as_secs()
    );
    (
        StatusCode::OK,
        [(header::SET_COOKIE, cookie)],
        Json(json!({ "ok": true })),
    )
        .into_response()
}

async fn logout(State(st): State<Arc<UiState>>, req: Request) -> Response {
    if let Some(t) = UiState::token_from(&req) {
        st.close_session(&t);
    }
    let cookie = format!("{COOKIE_NAME}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0");
    (
        StatusCode::OK,
        [(header::SET_COOKIE, cookie)],
        Json(json!({ "ok": true })),
    )
        .into_response()
}

// ----------------------------------------------------------------- 只读

async fn status(State(st): State<Arc<UiState>>) -> Response {
    Json(json!({
        "health": st.admin.health(),
        "settings": st.admin.settings(),
        "egresses": st.admin.egresses(),
        "models": st.admin.models(),
    }))
    .into_response()
}

async fn health(State(st): State<Arc<UiState>>) -> Response {
    Json(json!(st.admin.health())).into_response()
}

async fn models(State(st): State<Arc<UiState>>) -> Response {
    Json(json!({ "models": st.admin.models() })).into_response()
}

async fn egresses(State(st): State<Arc<UiState>>) -> Response {
    Json(json!({ "egresses": st.admin.egresses() })).into_response()
}

async fn proxies(State(st): State<Arc<UiState>>) -> Response {
    Json(json!({ "proxies": st.admin.settings().proxies })).into_response()
}

#[derive(Deserialize)]
struct LogsQ {
    lines: Option<usize>,
}

async fn logs(State(st): State<Arc<UiState>>, Query(q): Query<LogsQ>) -> Response {
    let lines = q.lines.unwrap_or(100).clamp(1, 500);
    Json(json!({ "logs": st.admin.logs(lines) })).into_response()
}

// ----------------------------------------------------------------- 写操作

#[derive(Default, Deserialize)]
struct SettingsBody {
    default_model: Option<String>,
    max_concurrency: Option<usize>,
}

async fn update_settings(
    State(st): State<Arc<UiState>>,
    Json(body): Json<SettingsBody>,
) -> Response {
    if let Some(m) = body.default_model.filter(|s| !s.trim().is_empty()) {
        if let Err(e) = st.admin.set_default_model(m) {
            return err(StatusCode::BAD_REQUEST, &e);
        }
    }
    if let Some(n) = body.max_concurrency {
        if let Err(e) = st.admin.set_max_concurrency(n) {
            return err(StatusCode::BAD_REQUEST, &e);
        }
    }
    Json(json!(st.admin.settings())).into_response()
}

#[derive(Deserialize)]
struct ProxyBody {
    url: String,
}

async fn add_proxy(State(st): State<Arc<UiState>>, Json(body): Json<ProxyBody>) -> Response {
    match st.admin.add_proxy(body.url) {
        Ok(()) => Json(json!({ "proxies": st.admin.settings().proxies })).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, &e),
    }
}

#[derive(Deserialize)]
struct ProxyQ {
    url: String,
}

async fn remove_proxy(State(st): State<Arc<UiState>>, Query(q): Query<ProxyQ>) -> Response {
    match st.admin.remove_proxy(q.url) {
        Ok(()) => Json(json!({ "proxies": st.admin.settings().proxies })).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, &e),
    }
}

#[derive(Deserialize)]
struct EgressBody {
    index: usize,
}

async fn ban_egress(State(st): State<Arc<UiState>>, Json(body): Json<EgressBody>) -> Response {
    match st.admin.ban_egress(body.index) {
        Ok(()) => Json(json!({ "egresses": st.admin.egresses() })).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, &e),
    }
}

async fn unban_egress(State(st): State<Arc<UiState>>, Json(body): Json<EgressBody>) -> Response {
    match st.admin.unban_egress(body.index) {
        Ok(()) => Json(json!({ "egresses": st.admin.egresses() })).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, &e),
    }
}

async fn trigger_probe(State(st): State<Arc<UiState>>) -> Response {
    st.admin.trigger_probe();
    (StatusCode::ACCEPTED, Json(json!({ "ok": true }))).into_response()
}

// ----------------------------------------------------------------- C2：密钥 / 出口策略 / 设置 / 口令

/// `GET /keys`：密钥列表（只回显前缀，绝不回显明文）。
async fn list_keys(State(st): State<Arc<UiState>>) -> Response {
    Json(json!({ "keys": st.admin.list_api_keys() })).into_response()
}

#[derive(Deserialize)]
struct CreateKeyBody {
    label: Option<String>,
}

/// `POST /keys`：创建密钥；返回的一次性明文 `key` 仅此可见。
async fn create_key(State(st): State<Arc<UiState>>, Json(body): Json<CreateKeyBody>) -> Response {
    match st.admin.create_api_key(body.label.unwrap_or_default()) {
        Ok(raw) => Json(json!({ "key": raw, "keys": st.admin.list_api_keys() })).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, &e),
    }
}

/// `DELETE /keys/{id}`：吊销（重复吊销返回 404）。
async fn revoke_key(State(st): State<Arc<UiState>>, Path(id): Path<i64>) -> Response {
    match st.admin.revoke_api_key(id) {
        Ok(true) => Json(json!({ "ok": true, "keys": st.admin.list_api_keys() })).into_response(),
        Ok(false) => err(StatusCode::NOT_FOUND, "密钥不存在"),
        Err(e) => err(StatusCode::BAD_REQUEST, &e),
    }
}

#[derive(Deserialize)]
struct PolicyBody {
    index: usize,
    /// 目标策略全量（含 enabled / cooldown_enabled / 三段时长）。
    policy: EgressPolicy,
}

/// `POST /egress/policy`：按槽位写 per-egress 策略（先落库，再改活池）。
async fn set_egress_policy(
    State(st): State<Arc<UiState>>,
    Json(body): Json<PolicyBody>,
) -> Response {
    match st.admin.set_egress_policy(body.index, body.policy) {
        Ok(()) => Json(json!({ "ok": true, "egresses": st.admin.egresses() })).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, &e),
    }
}

#[derive(Deserialize)]
struct DirectBody {
    enabled: bool,
}

/// `POST /egress/direct`：显式开关直连出口（增删 direct 行 + 活池同步）。
async fn set_direct_egress(
    State(st): State<Arc<UiState>>,
    Json(body): Json<DirectBody>,
) -> Response {
    match st.admin.set_direct_egress(body.enabled) {
        Ok(()) => Json(json!({ "ok": true, "egresses": st.admin.egresses() })).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, &e),
    }
}

#[derive(Deserialize)]
struct SaveSettingsBody {
    base: String,
    vqd_override: Option<String>,
    new_chat: Option<bool>,
}

/// `POST /settings/save`：设置保存（写穿库；返回生效时机说明）。
async fn save_settings(
    State(st): State<Arc<UiState>>,
    Json(body): Json<SaveSettingsBody>,
) -> Response {
    match st.admin.save_settings(
        body.base,
        body.vqd_override.unwrap_or_default(),
        body.new_chat.unwrap_or(false),
    ) {
        Ok(note) => Json(json!({ "ok": true, "note": note, "settings": st.admin.settings() }))
            .into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, &e),
    }
}

#[derive(Deserialize)]
struct PasswordBody {
    old: Option<String>,
    new: String,
}

/// `POST /password`：改管理口令（旧口令校验 + 最短 8 位；改后会话仍有效）。
async fn change_password(
    State(st): State<Arc<UiState>>,
    Json(body): Json<PasswordBody>,
) -> Response {
    match st
        .admin
        .change_admin_password(body.old.unwrap_or_default(), body.new)
    {
        Ok(()) => Json(json!({ "ok": true, "settings": st.admin.settings() })).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, &e),
    }
}

// ----------------------------------------------------------------- 装配

/// 路由声明（不含鉴权层）；`auth_guard` 由 `lib::router` 以真实状态挂载。
pub fn routes() -> Router<Arc<UiState>> {
    Router::new()
        .route("/login", post(login))
        .route("/logout", post(logout))
        .route("/status", get(status))
        .route("/health", get(health))
        .route("/models", get(models))
        .route("/egress", get(egresses))
        .route("/proxies", get(proxies))
        .route("/logs", get(logs))
        .route("/settings", post(update_settings))
        .route("/settings/save", post(save_settings))
        .route("/password", post(change_password))
        .route("/keys", get(list_keys).post(create_key))
        .route("/keys/{id}", delete(revoke_key))
        .route("/proxy", post(add_proxy).delete(remove_proxy))
        .route("/egress/ban", post(ban_egress))
        .route("/egress/unban", post(unban_egress))
        .route("/egress/policy", post(set_egress_policy))
        .route("/egress/direct", post(set_direct_egress))
        .route("/probe", post(trigger_probe))
}
