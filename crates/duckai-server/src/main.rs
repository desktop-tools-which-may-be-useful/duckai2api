//! duckai2api 可执行入口：读配置 → 打开 sqlite 库并装配 → 监听 → 优雅退出。
//!
//! 任何启动失败（含配置解析错误、库引导失败、非回环绑定缺 key 的 fail-fast）
//! 在此打印后退出码 1。

use std::net::SocketAddr;

use duckai_server::ServerConfig;

#[tokio::main]
async fn main() {
    let cfg = match ServerConfig::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("配置错误：{e}");
            eprintln!("（参考 .env.example；.env 与环境变量同时存在时环境变量优先）");
            std::process::exit(1);
        }
    };

    // RUST_LOG 消费点：EnvFilter 读 RUST_LOG，缺省回落配置里的 log_filter（默认 info）。
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(cfg.log_filter.clone()));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let app = match duckai_server::build(cfg) {
        Ok(a) => a,
        Err(e) => {
            tracing::error!("启动失败：{e}");
            std::process::exit(1);
        }
    };
    let addr = app.addr.clone();
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("监听 {addr} 失败：{e}");
            std::process::exit(1);
        }
    };
    tracing::info!("duckai2api 已启动：http://{addr}（/health 免鉴权，/v1 需 Bearer，WebUI 在 /）");

    let service = app
        .router
        .into_make_service_with_connect_info::<SocketAddr>();
    if let Err(e) = axum::serve(listener, service)
        .with_graceful_shutdown(shutdown_signal())
        .await
    {
        tracing::error!("服务退出异常：{e}");
        std::process::exit(1);
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("收到退出信号，正在关闭…");
}
