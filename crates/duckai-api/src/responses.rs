//! `POST /v1/responses`（Responses 形状：非流式 + SSE 事件流 + 工具编排）。
//! 原项目该端点恒 500（死代码遮蔽 NameError），此为 P0-1 回归位。

use std::collections::VecDeque;
use std::time::Instant;

use crate::gate::GatePermit;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use futures::stream;
use serde_json::{Value, json};

use duckai_upstream::UpstreamStream;

use crate::ApiState;
use crate::error::{ApiErr, Proto};
use crate::logs::now_secs;
use crate::openai::{prep_model_fallback, routed_collected};
use crate::prep::{Prepared, log_finish, prepare, start_chat};
use crate::resp::{error_response, json_response, sse_response};
use crate::stream::{Collected, StreamMsg, TextGate, collect, hex24, pump};

const PROTO_NAME: &str = "responses";

fn resp_id() -> String {
    format!("resp_{}", hex24())
}

fn msg_item_id() -> String {
    format!("msg_{}", hex24())
}

fn frame(event: &str, data: Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

pub async fn create(State(state): State<ApiState>, body: Bytes) -> Response {
    let started = Instant::now();
    let proto = Proto::Responses;

    let raw: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return error_response(proto, &ApiErr::Invalid(format!("请求体不是合法 JSON：{e}")));
        }
    };
    let prep = match prepare(&state, raw, proto).await {
        Ok(p) => p,
        Err(e) => {
            log_finish(
                &state,
                PROTO_NAME,
                &prep_model_fallback(&state, &e),
                started,
                e.status().as_u16(),
                Some(e.code()),
            );
            return error_response(proto, &e);
        }
    };

    if let Some(col) = routed_collected(proto, &prep) {
        log_finish(&state, PROTO_NAME, &prep.model, started, 200, None);
        return if prep.stream {
            responses_stream_response(&prep.model, col)
        } else {
            json_response(StatusCode::OK, response_json(&prep.model, &col))
        };
    }

    let (upstream, permit) = match start_chat(&state, &prep).await {
        Ok(v) => v,
        Err(e) => {
            log_finish(
                &state,
                PROTO_NAME,
                &prep.model,
                started,
                e.status().as_u16(),
                Some(e.code()),
            );
            return error_response(proto, &e);
        }
    };

    if prep.stream {
        responses_stream(state, prep, upstream, permit, started)
    } else {
        responses_nonstream(state, prep, upstream, permit, started).await
    }
}

/// 收集结果 → Responses `output` 数组。
/// message 恒在 index 0（与流式 output_item 事件的 output_index 对齐），工具调用随后。
fn output_items(col: &Collected, item_id: &str) -> Vec<Value> {
    let mut items = Vec::new();
    items.push(json!({
        "type": "message",
        "id": item_id,
        "status": "completed",
        "role": "assistant",
        "content": [{ "type": "output_text", "text": col.text }],
    }));
    if let Some((tid, name, args)) = col.tool() {
        items.push(json!({
            "type": "function_call",
            "id": tid,
            "call_id": tid,
            "name": name,
            "arguments": args.to_string(),
            "status": "completed",
        }));
    }
    items
}

fn response_json(model: &str, col: &Collected) -> Value {
    json!({
        "id": resp_id(),
        "object": "response",
        "created_at": now_secs(),
        "status": "completed",
        "model": model,
        "output": output_items(col, &msg_item_id()),
        "usage": { "input_tokens": 0, "output_tokens": 0, "total_tokens": 0 },
    })
}

async fn responses_nonstream(
    state: ApiState,
    prep: Prepared,
    mut upstream: UpstreamStream,
    _permit: GatePermit,
    started: Instant,
) -> Response {
    let tools_enabled = prep.tools_enabled();
    let col = match collect(&mut upstream, tools_enabled).await {
        Ok(c) => c,
        Err(e) => {
            log_finish(
                &state,
                PROTO_NAME,
                &prep.model,
                started,
                e.status().as_u16(),
                Some(e.code()),
            );
            return error_response(Proto::Responses, &e);
        }
    };
    log_finish(&state, PROTO_NAME, &prep.model, started, 200, None);
    json_response(StatusCode::OK, response_json(&prep.model, &col))
}

fn response_created(id: &str, model: &str) -> String {
    frame(
        "response.created",
        json!({
            "type": "response.created",
            "response": base_response(id, model, "in_progress"),
        }),
    )
}

fn response_in_progress(id: &str, model: &str) -> String {
    frame(
        "response.in_progress",
        json!({
            "type": "response.in_progress",
            "response": base_response(id, model, "in_progress"),
        }),
    )
}

fn base_response(id: &str, model: &str, status: &str) -> Value {
    json!({
        "id": id,
        "object": "response",
        "created_at": now_secs(),
        "status": status,
        "model": model,
        "output": [],
        "usage": Value::Null,
    })
}

fn text_delta(item_id: &str, text: &str) -> String {
    frame(
        "response.output_text.delta",
        json!({
            "type": "response.output_text.delta",
            "item_id": item_id,
            "output_index": 0,
            "content_index": 0,
            "delta": text,
        }),
    )
}

fn output_item_added(output_index: usize, item: Value) -> String {
    frame(
        "response.output_item.added",
        json!({
            "type": "response.output_item.added",
            "output_index": output_index,
            "item": item,
        }),
    )
}

fn output_item_done(output_index: usize, item: Value) -> String {
    frame(
        "response.output_item.done",
        json!({
            "type": "response.output_item.done",
            "output_index": output_index,
            "item": item,
        }),
    )
}

fn function_call_arguments_delta(item_id: &str, output_index: usize, partial: &str) -> String {
    frame(
        "response.function_call_arguments.delta",
        json!({
            "type": "response.function_call_arguments.delta",
            "item_id": item_id,
            "output_index": output_index,
            "delta": partial,
        }),
    )
}

fn function_call_arguments_done(item_id: &str, output_index: usize) -> String {
    frame(
        "response.function_call_arguments.done",
        json!({
            "type": "response.function_call_arguments.done",
            "item_id": item_id,
            "output_index": output_index,
        }),
    )
}

fn response_completed(id: &str, model: &str, col: &Collected, item_id: &str) -> String {
    frame(
        "response.completed",
        json!({
            "type": "response.completed",
            "response": {
                "id": id,
                "object": "response",
                "created_at": now_secs(),
                "status": "completed",
                "model": model,
                "output": output_items(col, item_id),
                "usage": { "input_tokens": 0, "output_tokens": 0, "total_tokens": 0 },
            },
        }),
    )
}

/// 路由合成流（不打上游）。
fn responses_stream_response(model: &str, col: Collected) -> Response {
    let id = resp_id();
    let item_id = msg_item_id();
    let mut frames = VecDeque::new();
    frames.push_back(response_created(&id, model));
    frames.push_back(response_in_progress(&id, model));
    if col.tool().is_none() && !col.text.is_empty() {
        frames.push_back(text_delta(&item_id, &col.text));
    }
    if let Some((tid, name, args)) = col.tool() {
        let fc = json!({
            "type": "function_call",
            "id": tid,
            "call_id": tid,
            "name": name,
            "arguments": "",
            "status": "in_progress",
        });
        frames.push_back(output_item_added(1, fc.clone()));
        frames.push_back(function_call_arguments_delta(&tid, 1, &args.to_string()));
        let mut done = fc;
        done["arguments"] = json!(args.to_string());
        done["status"] = json!("completed");
        frames.push_back(function_call_arguments_done(&tid, 1));
        frames.push_back(output_item_done(1, done));
    }
    frames.push_back(response_completed(&id, model, &col, &item_id));
    sse_response(stream::iter(frames))
}

struct StreamCtx {
    up: UpstreamStream,
    gate: TextGate,
    _permit: GatePermit,
    id: String,
    model: String,
    item_id: String,
    pending: VecDeque<String>,
    output_index: usize,
    /// 全量文本（delta 已逐帧外发；completed 的 output 需要全文）。
    full_text: String,
    /// 结构化工具事件（ToolCall 帧已外发 output_item 事件）。
    tool_event: Option<(String, String, Value)>,
    ended: bool,
    state: ApiState,
    started: Instant,
}

fn responses_stream(
    state: ApiState,
    prep: Prepared,
    upstream: UpstreamStream,
    permit: GatePermit,
    started: Instant,
) -> Response {
    let id = resp_id();
    let mut pending = VecDeque::new();
    pending.push_back(response_created(&id, &prep.model));
    pending.push_back(response_in_progress(&id, &prep.model));
    let ctx = StreamCtx {
        up: upstream,
        gate: TextGate::new(prep.tools_enabled()),
        _permit: permit,
        id,
        model: prep.model.clone(),
        item_id: msg_item_id(),
        pending,
        output_index: 1,
        full_text: String::new(),
        tool_event: None,
        ended: false,
        state,
        started,
    };
    sse_response(stream::unfold(ctx, |mut ctx| async move {
        loop {
            if let Some(frame) = ctx.pending.pop_front() {
                return Some((frame, ctx));
            }
            if ctx.ended {
                return None;
            }
            let msg = match pump(&mut ctx.up, &mut ctx.gate).await {
                None => Some(Ok(StreamMsg::End(ctx.gate.end("stop")))),
                other => other,
            };
            match msg {
                None => unreachable!("pump 的 None 已折叠"),
                Some(Err(e)) => {
                    log_finish(
                        &ctx.state,
                        PROTO_NAME,
                        &ctx.model,
                        ctx.started,
                        e.status().as_u16(),
                        Some(e.code()),
                    );
                    ctx.pending.push_back(frame("error", e.openai_body()));
                    ctx.ended = true;
                }
                Some(Ok(msg)) => match msg {
                    StreamMsg::Text(t) => {
                        ctx.full_text.push_str(&t);
                        ctx.pending.push_back(text_delta(&ctx.item_id, &t));
                    }
                    StreamMsg::Reasoning(_) => {}
                    StreamMsg::ToolCall {
                        id: tid,
                        name,
                        arguments,
                    } => {
                        let oi = ctx.output_index;
                        ctx.output_index += 1;
                        let args_str = arguments.to_string();
                        ctx.tool_event = Some((tid.clone(), name.clone(), arguments.clone()));
                        ctx.pending.push_back(output_item_added(
                            oi,
                            json!({
                                "type": "function_call",
                                "id": tid,
                                "call_id": tid,
                                "name": name,
                                "arguments": "",
                                "status": "in_progress",
                            }),
                        ));
                        ctx.pending
                            .push_back(function_call_arguments_delta(&tid, oi, &args_str));
                        ctx.pending
                            .push_back(function_call_arguments_done(&tid, oi));
                        ctx.pending.push_back(output_item_done(
                            oi,
                            json!({
                                "type": "function_call",
                                "id": tid,
                                "call_id": tid,
                                "name": name,
                                "arguments": args_str,
                                "status": "completed",
                            }),
                        ));
                    }
                    StreamMsg::Ping => continue,
                    StreamMsg::End(end) => {
                        ctx.full_text.push_str(&end.tail);
                        if !end.tail.is_empty() {
                            ctx.pending.push_back(text_delta(&ctx.item_id, &end.tail));
                        }
                        let mut col = Collected {
                            text: ctx.full_text.clone(),
                            tool_event: ctx.tool_event.clone(),
                            tool_envelope: end.tool,
                            reason: end.reason,
                        };
                        // 信封工具：结构化事件缺席时才补发 output_item 事件。
                        if ctx.tool_event.is_none() {
                            if let Some((tid, name, args)) = col.tool() {
                                let oi = ctx.output_index;
                                ctx.output_index += 1;
                                let args_str = args.to_string();
                                ctx.pending.push_back(output_item_added(
                                    oi,
                                    json!({
                                        "type": "function_call",
                                        "id": tid,
                                        "call_id": tid,
                                        "name": name,
                                        "arguments": "",
                                        "status": "in_progress",
                                    }),
                                ));
                                ctx.pending
                                    .push_back(function_call_arguments_delta(&tid, oi, &args_str));
                                ctx.pending
                                    .push_back(function_call_arguments_done(&tid, oi));
                                ctx.pending.push_back(output_item_done(
                                    oi,
                                    json!({
                                        "type": "function_call",
                                        "id": tid,
                                        "call_id": tid,
                                        "name": name,
                                        "arguments": args_str,
                                        "status": "completed",
                                    }),
                                ));
                            }
                        }
                        ctx.pending.push_back(response_completed(
                            &ctx.id,
                            &ctx.model,
                            &col,
                            &ctx.item_id,
                        ));
                        col.text.clear();
                        log_finish(&ctx.state, PROTO_NAME, &ctx.model, ctx.started, 200, None);
                        ctx.ended = true;
                    }
                },
            }
        }
    }))
}
