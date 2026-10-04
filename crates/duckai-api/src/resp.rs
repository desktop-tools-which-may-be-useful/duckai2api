//! 统一 HTTP 响应装配：规范错误帧（含 `retry-after`）、JSON、SSE。
use std::convert::Infallible;

use axum::body::Body;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, WWW_AUTHENTICATE};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use serde_json::Value;

use crate::error::{ApiErr, Proto};

/// 错误响应：协议族决定错误帧形状；429/503 一律带 `retry-after`；401 带 `WWW-Authenticate`。
pub fn error_response(proto: Proto, err: &ApiErr) -> Response {
    let mut resp = (
        err.status(),
        [(CONTENT_TYPE, "application/json; charset=utf-8")],
        axum::Json(err.body_for(proto)),
    )
        .into_response();
    if let Some(ra) = err.retry_after() {
        if let Ok(v) = HeaderValue::from_str(&ra.to_string()) {
            resp.headers_mut().insert(HDR_RETRY_AFTER, v);
        }
    }
    if matches!(err, ApiErr::Unauthorized) {
        if let Ok(v) = HeaderValue::from_str("Bearer") {
            resp.headers_mut().insert(WWW_AUTHENTICATE, v);
        }
    }
    resp
}

const HDR_RETRY_AFTER: HeaderName = HeaderName::from_static("retry-after");

/// 闸门耗尽 / 挑战失败的默认 `retry-after` 秒数。
pub const BUSY_RETRY_AFTER: u64 = 5;

/// 成功 JSON 响应。
pub fn json_response(status: StatusCode, body: Value) -> Response {
    (
        status,
        [(CONTENT_TYPE, "application/json; charset=utf-8")],
        axum::Json(body),
    )
        .into_response()
}

/// SSE 响应（`text/event-stream`，禁缓存）。帧流由调用方保证只含 `Ok`。
pub fn sse_response<S>(stream: S) -> Response
where
    S: futures::Stream<Item = String> + Send + 'static,
{
    let body = Body::from_stream(
        stream.map(|frame| Ok::<axum::body::Bytes, Infallible>(axum::body::Bytes::from(frame))),
    );
    (
        [
            (CONTENT_TYPE, "text/event-stream"),
            (CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}
