//! WebUI 层：恰好两份静态资源（无构建链）+ 管理 API。
//!
//! 分层约束：本 crate 只依赖 `duckai-types` 的 `AdminControl` trait，
//! 不感知 API 层与协议层的任何内部类型。

mod admin_api;
mod assets;

use std::sync::Arc;

use axum::Router;
use axum::middleware;

pub use admin_api::UiState;

/// 组装 WebUI 路由：
/// - `/`、`/index.html`、`/ui/*` → 静态资源（rust-embed，中文控制台）
/// - `/admin/api/*` → 管理 API（cookie 会话；无口令时回环只读、写恒 403）
pub fn router(
    admin: Arc<dyn duckai_types::AdminControl>,
    admin_password: Option<String>,
) -> Router {
    let state = Arc::new(UiState::new(admin, admin_password));
    let admin_routes = admin_api::routes().layer(middleware::from_fn_with_state(
        state.clone(),
        admin_api::auth_guard,
    ));
    Router::new()
        .merge(assets::router())
        .nest("/admin/api", admin_routes)
        .with_state(state)
}
