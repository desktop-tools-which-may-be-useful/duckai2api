//! API 契约测试（ARCHITECTURE §340）：
//! 三协议 × {流式/非流式/工具/错误注入}、P0-1 回归、429/503 头、鉴权矩阵、SSE 逐帧。

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use futures::StreamExt;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use duckai_api::ApiState;
use duckai_types::{EgressScope, ModelInfo, UpstreamError, UpstreamEvent};
use duckai_upstream::{MockUpstream, ScriptedTurn};

const KEY: &str = "sk-test-key";

fn catalog() -> Vec<ModelInfo> {
    vec![
        ModelInfo::snapshot("mock-model", "duck.ai"),
        ModelInfo::snapshot("other-model", "tinfoil"),
    ]
}

fn state(mock: Arc<MockUpstream>, api_key: Option<&str>, max: usize) -> ApiState {
    ApiState::new(
        mock,
        api_key.map(str::to_string),
        "mock-model".to_string(),
        max,
    )
}

/// 组装 app：`app(mock, KEY, 8)`；鉴权关闭传 `None`。
fn app(mock: Arc<MockUpstream>, api_key: Option<&str>, max: usize) -> Router {
    duckai_api::router(state(mock, api_key, max))
}

fn mock(script: Vec<ScriptedTurn>) -> Arc<MockUpstream> {
    let m = Arc::new(MockUpstream::new().with_models(catalog()));
    for t in script {
        m.push(t);
    }
    m
}

fn body_of(value: &Value) -> String {
    value.to_string()
}

async fn oneshot(app: &Router, req: Request<Body>) -> axum::response::Response {
    app.clone().oneshot(req).await.expect("router 应答")
}

/// 发 JSON 并整体收集 → (status, headers, body JSON)。
async fn post_json(
    app: &Router,
    path: &str,
    bearer: Option<&str>,
    payload: Value,
) -> (StatusCode, axum::http::HeaderMap, Value) {
    let mut builder = Request::post(path).header(header::CONTENT_TYPE, "application/json");
    if let Some(k) = bearer {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {k}"));
    }
    let resp = oneshot(app, builder.body(Body::from(body_of(&payload))).unwrap()).await;
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, headers, value)
}

/// SSE：整体读完 → (status, frames)。
async fn post_sse(
    app: &Router,
    path: &str,
    bearer: Option<&str>,
    payload: Value,
) -> (StatusCode, Vec<String>) {
    let mut builder = Request::post(path).header(header::CONTENT_TYPE, "application/json");
    if let Some(k) = bearer {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {k}"));
    }
    let resp = oneshot(app, builder.body(Body::from(body_of(&payload))).unwrap()).await;
    let status = resp.status();
    let mut buf = String::new();
    let mut stream = resp.into_body().into_data_stream();
    while let Some(chunk) = stream.next().await {
        buf.push_str(std::str::from_utf8(&chunk.unwrap()).expect("UTF-8 帧"));
    }
    let frames = buf
        .split("\n\n")
        .filter(|f| !f.trim().is_empty())
        .map(str::to_string)
        .collect();
    (status, frames)
}

/// 从 `data: ` 行取 JSON（跳过 [DONE]）。
fn data_json(frame: &str) -> Option<Value> {
    frame
        .lines()
        .find_map(|l| l.strip_prefix("data: "))
        .and_then(|d| {
            if d == "[DONE]" {
                None
            } else {
                Some(serde_json::from_str(d).expect("帧应为 JSON"))
            }
        })
}

/// Anthropic/Responses 的 `event:` 名。
fn event_name(frame: &str) -> Option<String> {
    frame
        .lines()
        .find_map(|l| l.strip_prefix("event: "))
        .map(str::to_string)
}

fn user_msg(text: &str) -> Value {
    json!({ "role": "user", "content": text })
}

fn read_tool() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "Read",
            "description": "读取文件内容",
            "parameters": { "type": "object", "properties": { "file_path": { "type": "string" } } },
        }
    })
}

// ---------------------------------------------------------------- 基础非流式

#[tokio::test]
async fn openai_nonstream_basic() {
    let m = mock(vec![ScriptedTurn::Chunked(
        vec![
            UpstreamEvent::TextDelta("你好，".into()),
            UpstreamEvent::TextDelta("世界".into()),
            UpstreamEvent::Done {
                finish_reason: "stop".into(),
            },
        ],
        Duration::from_millis(1),
    )]);
    let app = app(m.clone(), Some(KEY), 8);
    let (status, _, body) = post_json(
        &app,
        "/v1/chat/completions",
        Some(KEY),
        json!({ "model": "mock-model", "messages": [user_msg("hi")] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["id"].as_str().unwrap().starts_with("chatcmpl-"));
    assert_eq!(body["object"], "chat.completion");
    assert_eq!(body["model"], "mock-model");
    assert_eq!(body["choices"][0]["message"]["content"], "你好，世界");
    assert_eq!(body["choices"][0]["finish_reason"], "stop");
    assert_eq!(body["usage"]["total_tokens"], 0);

    let seen = m.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].model, "mock-model");
    assert_eq!(seen[0].turns.len(), 1);
}

/// 省略 model → 回落 DUCKAI_MODEL（配置消费点）。
#[tokio::test]
async fn openai_default_model_injected() {
    let m = mock(vec![]);
    let app = app(m.clone(), Some(KEY), 8);
    let (status, _, body) = post_json(
        &app,
        "/v1/chat/completions",
        Some(KEY),
        json!({ "messages": [user_msg("hi")] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["model"], "mock-model");
    assert_eq!(m.seen()[0].model, "mock-model");
}

// ---------------------------------------------------------------- 流式

#[tokio::test]
async fn openai_stream_frames_single_id() {
    let m = mock(vec![ScriptedTurn::Chunked(
        vec![
            UpstreamEvent::TextDelta("部分一".into()),
            UpstreamEvent::TextDelta("部分二".into()),
            UpstreamEvent::Done {
                finish_reason: "stop".into(),
            },
        ],
        Duration::from_millis(1),
    )]);
    let app = app(m, Some(KEY), 8);
    let (status, frames) = post_sse(
        &app,
        "/v1/chat/completions",
        Some(KEY),
        json!({ "model": "mock-model", "messages": [user_msg("hi")], "stream": true }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        frames.last().unwrap().ends_with("[DONE]"),
        "帧尾应为 [DONE]：{frames:?}"
    );

    let mut ids = Vec::new();
    let mut text = String::new();
    let mut finish = None;
    for f in &frames {
        if let Some(v) = data_json(f) {
            ids.push(v["id"].as_str().unwrap().to_string());
            if let Some(d) = v["choices"][0]["delta"].as_object() {
                if let Some(c) = d.get("content").and_then(Value::as_str) {
                    text.push_str(c);
                }
            }
            if let Some(r) = v["choices"][0]["finish_reason"].as_str() {
                finish = Some(r.to_string());
            }
            assert_eq!(v["object"], "chat.completion.chunk");
        }
    }
    assert!(ids.len() >= 4, "至少 role+2 delta+finish：{frames:?}");
    let first = &ids[0];
    assert!(
        ids.iter().all(|i| i == first),
        "同一 completion id 必须一致：{ids:?}"
    );
    assert!(first.starts_with("chatcmpl-"));
    assert_eq!(text, "部分一部分二");
    assert_eq!(finish.as_deref(), Some("stop"));
}

#[tokio::test]
async fn anthropic_stream_frame_order() {
    let m = mock(vec![ScriptedTurn::Chunked(
        vec![
            UpstreamEvent::TextDelta("甲".into()),
            UpstreamEvent::TextDelta("乙".into()),
            UpstreamEvent::Done {
                finish_reason: "stop".into(),
            },
        ],
        Duration::from_millis(1),
    )]);
    let app = app(m, Some(KEY), 8);
    let (status, frames) = post_sse(
        &app,
        "/v1/messages",
        Some(KEY),
        json!({
            "model": "mock-model",
            "max_tokens": 16,
            "messages": [user_msg("hi")],
            "stream": true,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let events: Vec<String> = frames.iter().filter_map(|f| event_name(f)).collect();
    assert_eq!(
        events,
        vec![
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop",
        ],
        "帧序必须严格：{frames:?}"
    );
    // 断言数据内容
    let start = data_json(&frames[0]).expect("message_start data");
    assert!(start["message"]["id"].as_str().unwrap().starts_with("msg_"));
    assert_eq!(start["message"]["role"], "assistant");
    let d1 = data_json(&frames[2]).unwrap();
    assert_eq!(d1["delta"]["type"], "text_delta");
    assert_eq!(d1["delta"]["text"], "甲");
    let md = data_json(&frames[5]).unwrap();
    assert_eq!(md["delta"]["stop_reason"], "end_turn");
}

#[tokio::test]
async fn anthropic_nonstream_end_turn() {
    let m = mock(vec![ScriptedTurn::Chunked(
        vec![
            UpstreamEvent::TextDelta("你好".into()),
            UpstreamEvent::Done {
                finish_reason: "stop".into(),
            },
        ],
        Duration::from_millis(1),
    )]);
    let app = app(m, Some(KEY), 8);
    let (status, _, body) = post_json(
        &app,
        "/v1/messages",
        Some(KEY),
        json!({ "model": "mock-model", "max_tokens": 16, "messages": [user_msg("hi")] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["type"], "message");
    assert_eq!(body["role"], "assistant");
    assert!(body["id"].as_str().unwrap().starts_with("msg_"));
    assert_eq!(body["content"][0]["type"], "text");
    assert_eq!(body["content"][0]["text"], "你好");
    assert_eq!(body["stop_reason"], "end_turn");
}

// ---------------------------------------------------------------- P0-1 Responses

#[tokio::test]
async fn responses_p01_string_input() {
    // 原项目此端点恒 500（P0-1）：字符串 input 必须可用。
    let m = mock(vec![ScriptedTurn::Chunked(
        vec![
            UpstreamEvent::TextDelta("世界你好".into()),
            UpstreamEvent::Done {
                finish_reason: "stop".into(),
            },
        ],
        Duration::from_millis(1),
    )]);
    let app = app(m, Some(KEY), 8);
    let (status, _, body) = post_json(
        &app,
        "/v1/responses",
        Some(KEY),
        json!({ "model": "mock-model", "input": "你好" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "completed");
    assert!(body["id"].as_str().unwrap().starts_with("resp_"));
    assert_eq!(body["output"][0]["content"][0]["text"], "世界你好");
}

#[tokio::test]
async fn responses_message_list_input() {
    let m = mock(vec![ScriptedTurn::Chunked(
        vec![UpstreamEvent::Done {
            finish_reason: "stop".into(),
        }],
        Duration::from_millis(1),
    )]);
    let app = app(m.clone(), Some(KEY), 8);
    let (status, _, body) = post_json(
        &app,
        "/v1/responses",
        Some(KEY),
        json!({
            "model": "mock-model",
            "input": [{ "role": "user", "content": [{ "type": "input_text", "text": "在吗" }] }],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let seen = m.seen();
    assert_eq!(seen[0].turns.len(), 1);
    assert_eq!(seen[0].turns[0].content.text(), "在吗");
}

#[tokio::test]
async fn responses_stream_created_to_completed() {
    let m = mock(vec![ScriptedTurn::Chunked(
        vec![
            UpstreamEvent::TextDelta("流式".into()),
            UpstreamEvent::Done {
                finish_reason: "stop".into(),
            },
        ],
        Duration::from_millis(1),
    )]);
    let app = app(m, Some(KEY), 8);
    let (status, frames) = post_sse(
        &app,
        "/v1/responses",
        Some(KEY),
        json!({ "model": "mock-model", "input": "hi", "stream": true }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let events: Vec<String> = frames.iter().filter_map(|f| event_name(f)).collect();
    assert_eq!(events.first().unwrap(), "response.created");
    assert_eq!(events.last().unwrap(), "response.completed");
    assert!(events.contains(&"response.output_text.delta".to_string()));
    let created = data_json(&frames[0]).unwrap();
    assert!(
        created["response"]["id"]
            .as_str()
            .unwrap()
            .starts_with("resp_")
    );
    let done = data_json(frames.last().unwrap()).unwrap();
    assert_eq!(done["response"]["status"], "completed");
    assert_eq!(done["response"]["output"][0]["content"][0]["text"], "流式");
}

// ---------------------------------------------------------------- 工具编排

#[tokio::test]
async fn openai_tool_routed_without_upstream() {
    // 话术直连：命中工具意图 → 不打上游（P0-2 核心断言）。
    let m = mock(vec![]);
    let app = app(m.clone(), Some(KEY), 8);
    let (status, _, body) = post_json(
        &app,
        "/v1/chat/completions",
        Some(KEY),
        json!({
            "model": "mock-model",
            "messages": [user_msg("Read config.toml for me")],
            "tools": [read_tool()],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["choices"][0]["finish_reason"], "tool_calls");
    let call = &body["choices"][0]["message"]["tool_calls"][0];
    assert_eq!(call["type"], "function");
    assert_eq!(call["function"]["name"], "Read");
    assert!(call["id"].as_str().unwrap().starts_with("call_"));
    assert!(m.seen().is_empty(), "路由直连不得访问上游");
}

#[tokio::test]
async fn anthropic_tool_routed() {
    let m = mock(vec![]);
    let app = app(m.clone(), Some(KEY), 8);
    let (status, _, body) = post_json(
        &app,
        "/v1/messages",
        Some(KEY),
        json!({
            "model": "mock-model",
            "max_tokens": 64,
            "messages": [user_msg("Please read config.toml")],
            "tools": [{ "name": "Read", "description": "读文件", "input_schema": { "type": "object" } }],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["stop_reason"], "tool_use");
    assert_eq!(body["content"][0]["type"], "tool_use");
    assert_eq!(body["content"][0]["name"], "Read");
    assert!(
        body["content"][0]["id"]
            .as_str()
            .unwrap()
            .starts_with("toolu_")
    );
    assert!(m.seen().is_empty());
}

#[tokio::test]
async fn responses_tool_routed() {
    let m = mock(vec![]);
    let app = app(m.clone(), Some(KEY), 8);
    let (status, _, body) = post_json(
        &app,
        "/v1/responses",
        Some(KEY),
        json!({
            "model": "mock-model",
            "input": "read config.toml please",
            "tools": [{ "type": "function", "name": "Read", "parameters": { "type": "object" } }],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "completed");
    let fc = body["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "function_call")
        .expect("function_call 输出项");
    assert_eq!(fc["name"], "Read");
    assert!(fc["call_id"].as_str().unwrap().starts_with("fc_"));
    assert!(m.seen().is_empty());
}

/// 非流式 + 上游结构化工具事件：信封注入到用户轮，工具调用解析正确。
#[tokio::test]
async fn openai_tool_envelope_injected_nonstream() {
    let m = mock(vec![ScriptedTurn::ToolCall {
        name: "Read".into(),
        arguments: json!({ "file_path": "a.txt" }),
    }]);
    let app = app(m.clone(), Some(KEY), 8);
    let (status, _, body) = post_json(
        &app,
        "/v1/chat/completions",
        Some(KEY),
        json!({
            "model": "mock-model",
            "messages": [user_msg("好的，开始吧")],
            "tools": [read_tool()],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(
        body["choices"][0]["message"]["content"], "调用工具中…",
        "前导正文应保留"
    );
    let call = &body["choices"][0]["message"]["tool_calls"][0];
    assert_eq!(call["function"]["name"], "Read");

    let seen = m.seen();
    assert_eq!(seen.len(), 1, "信封注入场景必须打上游");
    let last = seen[0].turns.last().unwrap();
    assert!(
        last.content.text().contains("<tool_call"),
        "用户轮必须注入工具信封：{}",
        last.content.text()
    );
    assert!(
        !last.content.text().contains("调用工具中…"),
        "信封注入发生在请求侧，不含上游正文"
    );
}

/// 流式 + 上游结构化工具：信封文本绝不外泄；tool_calls 增量帧化。
#[tokio::test]
async fn openai_tool_stream_hides_envelope() {
    let m = mock(vec![ScriptedTurn::ToolCall {
        name: "Read".into(),
        arguments: json!({ "file_path": "b.txt" }),
    }]);
    let app = app(m, Some(KEY), 8);
    let (status, frames) = post_sse(
        &app,
        "/v1/chat/completions",
        Some(KEY),
        json!({
            "model": "mock-model",
            "messages": [user_msg("好的，开始吧")],
            "tools": [read_tool()],
            "stream": true,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let all: String = frames.join("\n");
    assert!(!all.contains("<tool_call"), "信封标记不得外泄：{all}");
    assert!(all.contains("\"tool_calls\""), "应有 tool_calls 帧：{all}");
    let mut finish = None;
    for f in &frames {
        if let Some(v) = data_json(f) {
            if let Some(r) = v["choices"][0]["finish_reason"].as_str() {
                finish = Some(r.to_string());
            }
        }
    }
    assert_eq!(finish.as_deref(), Some("tool_calls"));
}

/// Anthropic 流式工具：input_json_delta 帧。
#[tokio::test]
async fn anthropic_tool_stream() {
    let m = mock(vec![ScriptedTurn::ToolCall {
        name: "Read".into(),
        arguments: json!({ "file_path": "c.txt" }),
    }]);
    let app = app(m, Some(KEY), 8);
    let (status, frames) = post_sse(
        &app,
        "/v1/messages",
        Some(KEY),
        json!({
            "model": "mock-model",
            "max_tokens": 64,
            "messages": [user_msg("好的，开始吧")],
            "tools": [{ "name": "Read", "description": "读文件", "input_schema": { "type": "object" } }],
            "stream": true,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let events: Vec<String> = frames.iter().filter_map(|f| event_name(f)).collect();
    assert!(events.contains(&"content_block_start".to_string()));
    let has_json_delta = frames
        .iter()
        .any(|f| data_json(f).is_some_and(|v| v["delta"]["type"] == "input_json_delta"));
    assert!(has_json_delta, "应有 input_json_delta：{frames:?}");
    let md = frames
        .iter()
        .filter_map(|f| data_json(f))
        .find(|v| v["type"] == "message_delta")
        .expect("message_delta");
    assert_eq!(md["delta"]["stop_reason"], "tool_use");
    assert!(!frames.join("").contains("<tool_call"));
}

// ---------------------------------------------------------------- 错误注入

#[tokio::test]
async fn rate_limited_429_with_retry_after() {
    let m = mock(vec![ScriptedTurn::Fail(UpstreamError::RateLimited {
        retry_after: 7,
    })]);
    let app = app(m, Some(KEY), 8);
    let (status, headers, body) = post_json(
        &app,
        "/v1/chat/completions",
        Some(KEY),
        json!({ "model": "mock-model", "messages": [user_msg("hi")] }),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(
        headers.get("retry-after").and_then(|v| v.to_str().ok()),
        Some("7")
    );
    assert_eq!(body["error"]["type"], "rate_limit_error");
    assert_eq!(body["error"]["code"], "rate_limit_exceeded");
}

#[tokio::test]
async fn banned_503_never_says_account_banned() {
    let m = mock(vec![ScriptedTurn::Fail(UpstreamError::Banned {
        scope: EgressScope::Direct,
    })]);
    let app = app(m, Some(KEY), 8);
    let (status, headers, body) = post_json(
        &app,
        "/v1/chat/completions",
        Some(KEY),
        json!({ "model": "mock-model", "messages": [user_msg("hi")] }),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(
        headers.get("retry-after").is_some(),
        "503 必须带 retry-after"
    );
    let msg = body["error"]["message"].as_str().unwrap();
    assert!(
        !msg.contains("账号被封"),
        "Banned 措辞不得断言账号封禁：{msg}"
    );
    assert_eq!(body["error"]["type"], "server_error");
}

#[tokio::test]
async fn anthropic_error_shape_on_503() {
    let m = mock(vec![ScriptedTurn::Fail(UpstreamError::Timeout)]);
    let app = app(m, Some(KEY), 8);
    let (status, _, body) = post_json(
        &app,
        "/v1/messages",
        Some(KEY),
        json!({ "model": "mock-model", "max_tokens": 8, "messages": [user_msg("hi")] }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "api_error");
}

#[tokio::test]
async fn model_not_found_404() {
    let m = mock(vec![]);
    let app = app(m, Some(KEY), 8);
    let (status, _, body) = post_json(
        &app,
        "/v1/chat/completions",
        Some(KEY),
        json!({ "model": "no-such-model", "messages": [user_msg("hi")] }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"]["code"], "model_not_found");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no-such-model")
    );
}

#[tokio::test]
async fn empty_prompt_400() {
    let m = mock(vec![]);
    let app = app(m, Some(KEY), 8);
    let (status, _, body) = post_json(
        &app,
        "/v1/chat/completions",
        Some(KEY),
        json!({ "model": "mock-model", "messages": [{ "role": "user", "content": "" }] }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["type"], "invalid_request_error");
}

// ---------------------------------------------------------------- 鉴权矩阵

#[tokio::test]
async fn auth_matrix_openai() {
    let m = mock(vec![]);
    let app = app(m, Some(KEY), 8);

    let (s, h, b) = post_json(
        &app,
        "/v1/chat/completions",
        Some("sk-wrong"),
        json!({ "messages": [user_msg("x")] }),
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "{b}");
    assert_eq!(
        h.get(header::WWW_AUTHENTICATE)
            .and_then(|v| v.to_str().ok()),
        Some("Bearer")
    );
    assert_eq!(b["error"]["code"], "invalid_api_key");

    let (s, _, b) = post_json(
        &app,
        "/v1/chat/completions",
        None,
        json!({ "messages": [user_msg("x")] }),
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "{b}");

    let (s, _, b) = post_json(
        &app,
        "/v1/chat/completions",
        Some(KEY),
        json!({ "messages": [user_msg("x")] }),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

#[tokio::test]
async fn auth_matrix_anthropic_shape() {
    let m = mock(vec![]);
    let app = app(m, Some(KEY), 8);
    let (s, _, b) = post_json(
        &app,
        "/v1/messages",
        Some("sk-wrong"),
        json!({ "messages": [user_msg("x")], "max_tokens": 8 }),
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "{b}");
    assert_eq!(b["type"], "error");
    assert_eq!(b["error"]["type"], "authentication_error");
}

#[tokio::test]
async fn auth_disabled_when_no_key() {
    let m = mock(vec![ScriptedTurn::Chunked(
        vec![UpstreamEvent::Done {
            finish_reason: "stop".into(),
        }],
        Duration::from_millis(1),
    )]);
    let app = app(m, None, 8);
    let (s, _, b) = post_json(
        &app,
        "/v1/chat/completions",
        None,
        json!({ "messages": [user_msg("x")] }),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

// ---------------------------------------------------------------- 并发闸门

#[tokio::test]
async fn concurrency_gate_429() {
    let m = mock(vec![ScriptedTurn::Hang]);
    let app = app(m, Some(KEY), 1);

    // 请求 1：流式挂住 → 持有唯一令牌（Body 未读不归还）。
    let resp1 = oneshot(
        &app,
        Request::post("/v1/chat/completions")
            .header(header::AUTHORIZATION, format!("Bearer {KEY}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({ "model": "mock-model", "messages": [user_msg("hi")], "stream": true })
                    .to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(resp1.status(), StatusCode::OK);

    // 请求 2：闸门耗尽 → 429 + retry-after。
    let (s, h, b) = post_json(
        &app,
        "/v1/chat/completions",
        Some(KEY),
        json!({ "model": "mock-model", "messages": [user_msg("again")] }),
    )
    .await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS, "{b}");
    assert_eq!(
        h.get("retry-after").and_then(|v| v.to_str().ok()),
        Some("5")
    );
    assert_eq!(b["error"]["type"], "rate_limit_error");

    drop(resp1); // 令牌归还
    // 闸门恢复可用（无需再发完整请求，直接查 inflight）
    assert_eq!(resp1_status_after_drop(&app).await, StatusCode::OK);
}

/// drop 掉挂住的响应后，新请求应重新拿到令牌。
async fn resp1_status_after_drop(app: &Router) -> StatusCode {
    let m_up = Request::post("/v1/chat/completions")
        .header(header::AUTHORIZATION, format!("Bearer {KEY}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({ "model": "mock-model", "messages": [user_msg("hi")] }).to_string(),
        ))
        .unwrap();
    oneshot(app, m_up).await.status()
}

// ---------------------------------------------------------------- 模型列表

#[tokio::test]
async fn models_endpoint() {
    let m = mock(vec![]);
    let app = app(m, Some(KEY), 8);
    let resp = oneshot(
        &app,
        Request::get("/v1/models")
            .header(header::AUTHORIZATION, format!("Bearer {KEY}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["object"], "list");
    let data = v["data"].as_array().unwrap();
    assert!(data.iter().any(|m| m["id"] == "mock-model"));
    assert!(data.iter().all(|m| m["object"] == "model"));
    assert!(data.iter().any(|m| m["aliases"].is_array()));
}

#[tokio::test]
async fn models_endpoint_requires_auth() {
    let m = mock(vec![]);
    let app = app(m, Some(KEY), 8);
    let resp = oneshot(
        &app,
        Request::get("/v1/models").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------- SSE 逐帧

#[tokio::test]
async fn sse_frames_arrive_incrementally() {
    let m = mock(vec![ScriptedTurn::Chunked(
        vec![
            UpstreamEvent::TextDelta("A".into()),
            UpstreamEvent::TextDelta("B".into()),
            UpstreamEvent::Done {
                finish_reason: "stop".into(),
            },
        ],
        Duration::from_millis(40),
    )]);
    let app = app(m, Some(KEY), 8);
    let resp = oneshot(
        &app,
        Request::post("/v1/chat/completions")
            .header(header::AUTHORIZATION, format!("Bearer {KEY}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({ "model": "mock-model", "messages": [user_msg("hi")], "stream": true })
                    .to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream")
    );

    let mut stream = resp.into_body().into_data_stream();
    let t0 = Instant::now();
    let first = stream.next().await.expect("首帧").unwrap();
    let t_first = t0.elapsed();
    let second = stream.next().await.expect("次帧").unwrap();
    let t_second = t0.elapsed();

    let first_txt = String::from_utf8_lossy(&first).to_string();
    assert!(
        first_txt.contains("chatcmpl-"),
        "首帧应为 role 帧：{first_txt}"
    );
    // 首帧在上游间隔（40ms）之前到达 → 真流式非整体缓冲。
    assert!(
        t_first < Duration::from_millis(35),
        "首帧应立即到达，实测 {t_first:?}"
    );
    assert!(
        t_second >= Duration::from_millis(30),
        "第二帧应等待上游间隔，实测 {t_second:?}"
    );
    let second_txt = String::from_utf8_lossy(&second).to_string();
    assert!(second_txt.contains('A'), "次帧应含首段正文：{second_txt}");
}

// ---------------------------------------------------------------- reasoning / 会话线索

#[tokio::test]
async fn reasoning_effort_and_session_hint_pass_through() {
    let m = mock(vec![]);
    let app = app(m.clone(), Some(KEY), 8);
    let (status, _, body) = post_json(
        &app,
        "/v1/chat/completions",
        Some(KEY),
        json!({
            "model": "mock-model",
            "messages": [user_msg("hi")],
            "reasoning_effort": "high",
            "user": "sess-42",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let seen = m.seen();
    assert_eq!(seen[0].reasoning_effort.as_deref(), Some("high"));
    assert_eq!(seen[0].session_hint.as_deref(), Some("sess-42"));
}
