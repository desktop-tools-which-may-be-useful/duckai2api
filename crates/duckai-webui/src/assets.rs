//! 静态资源嵌入（`ui/` 目录，恰好两份文件，无任何构建链）。

use axum::Router;
use axum::extract::Path;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use rust_embed::RustEmbed;
use std::sync::Arc;

use crate::UiState;

#[derive(RustEmbed)]
#[folder = "ui/"]
struct Assets;

fn mime(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "json" => "application/json; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn serve(rel: &str) -> Response {
    match Assets::get(rel) {
        Some(file) => ([(header::CONTENT_TYPE, mime(rel))], file.data.into_owned()).into_response(),
        None => (StatusCode::NOT_FOUND, "404 not found").into_response(),
    }
}

async fn index() -> Response {
    serve("index.html")
}

async fn ui_file(Path(rel): Path<String>) -> Response {
    serve(&rel)
}

pub fn router() -> Router<Arc<UiState>> {
    Router::new()
        .route("/", get(index))
        .route("/index.html", get(index))
        .route("/ui/{*rel}", get(ui_file))
}
