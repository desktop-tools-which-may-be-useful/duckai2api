//! `POST /v1/messages`（Anthropic 形状：非流式 + SSE 事件流 + 工具编排）。

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
use crate::openai::{prep_model_fallback, routed_collected};
use crate::prep::{Prepared, log_finish, prepare, start_chat};
use crate::resp::{error_response, json_response, sse_response};
use crate::stream::{Collected, StreamMsg, TextGate, collect, hex24, pump};

const PROTO_NAME: &str = "messages";

fn msg_id() -> String {
    format!("msg_{}", hex24())
}

/// Anthropic SSE 帧：`event:` + `data:` 成对。
fn frame(event: &str, data: Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

pub async fn messages(State(state): State<ApiState>, body: Bytes) -> Response {
    let started = Instant::now();
    let proto = Proto::Anthropic;

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
            anthropic_stream_response(&prep.model, col)
        } else {
            json_response(StatusCode::OK, message_json(&prep.model, &col))
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
        anthropic_stream(state, prep, upstream, permit, started)
    } else {
        anthropic_nonstream(state, prep, upstream, permit, started).await
    }
}

/// 收集结果 → Anthropic `content` 数组。
fn content_blocks(col: &Collected) -> Vec<Value> {
    let mut blocks = Vec::new();
    let pre = col.text.trim();
    if let Some((tid, name, args)) = col.tool() {
        if !pre.is_empty() {
            blocks.push(json!({ "type": "text", "text": pre }));
        }
        blocks.push(json!({
            "type": "tool_use",
            "id": tid,
            "name": name,
            "input": args,
        }));
    } else {
        blocks.push(json!({ "type": "text", "text": col.text }));
    }
    blocks
}

fn stop_reason(col: &Collected) -> &'static str {
    if col.tool().is_some() {
        "tool_use"
    } else {
        match col.reason.as_str() {
            "length" => "max_tokens",
            _ => "end_turn",
        }
    }
}

fn message_json(model: &str, col: &Collected) -> Value {
    json!({
        "id": msg_id(),
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content_blocks(col),
        "stop_reason": stop_reason(col),
        "stop_sequence": Value::Null,
        "usage": { "input_tokens": 0, "output_tokens": 0 },
    })
}

async fn anthropic_nonstream(
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
            return error_response(Proto::Anthropic, &e);
        }
    };
    log_finish(&state, PROTO_NAME, &prep.model, started, 200, None);
    json_response(StatusCode::OK, message_json(&prep.model, &col))
}

fn message_start(id: &str, model: &str) -> String {
    frame(
        "message_start",
        json!({
            "type": "message_start",
            "message": {
                "id": id,
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": [],
                "stop_reason": Value::Null,
                "stop_sequence": Value::Null,
                "usage": { "input_tokens": 0, "output_tokens": 0 },
            },
        }),
    )
}

fn text_start(index: usize) -> String {
    frame(
        "content_block_start",
        json!({
            "type": "content_block_start",
            "index": index,
            "content_block": { "type": "text", "text": "" },
        }),
    )
}

fn text_delta(index: usize, text: &str) -> String {
    frame(
        "content_block_delta",
        json!({
            "type": "content_block_delta",
            "index": index,
            "delta": { "type": "text_delta", "text": text },
        }),
    )
}

fn tool_start(index: usize, tid: &str, name: &str) -> String {
    frame(
        "content_block_start",
        json!({
            "type": "content_block_start",
            "index": index,
            "content_block": { "type": "tool_use", "id": tid, "name": name, "input": {} },
        }),
    )
}

fn tool_delta(index: usize, partial_json: &str) -> String {
    frame(
        "content_block_delta",
        json!({
            "type": "content_block_delta",
            "index": index,
            "delta": { "type": "input_json_delta", "partial_json": partial_json },
        }),
    )
}

fn block_stop(index: usize) -> String {
    frame(
        "content_block_stop",
        json!({ "type": "content_block_stop", "index": index }),
    )
}

fn message_delta(stop: &str) -> String {
    frame(
        "message_delta",
        json!({
            "type": "message_delta",
            "delta": { "stop_reason": stop, "stop_sequence": Value::Null },
            "usage": { "output_tokens": 0 },
        }),
    )
}

fn message_stop() -> String {
    frame("message_stop", json!({ "type": "message_stop" }))
}

/// 路由合成流（不打上游）。
fn anthropic_stream_response(model: &str, col: Collected) -> Response {
    let id = msg_id();
    let mut frames = VecDeque::new();
    frames.push_back(message_start(&id, model));
    let mut index = 0usize;
    if let Some((tid, name, args)) = col.tool() {
        frames.push_back(tool_start(index, &tid, &name));
        frames.push_back(tool_delta(index, &args.to_string()));
        frames.push_back(block_stop(index));
        index += 1;
    } else {
        frames.push_back(text_start(index));
        frames.push_back(text_delta(index, &col.text));
        frames.push_back(block_stop(index));
        index += 1;
    }
    let _ = index;
    frames.push_back(message_delta(stop_reason(&col)));
    frames.push_back(message_stop());
    sse_response(stream::iter(frames))
}

struct StreamCtx {
    up: UpstreamStream,
    gate: TextGate,
    _permit: GatePermit,
    model: String,
    pending: VecDeque<String>,
    next_index: usize,
    text_open: bool,
    ended: bool,
    state: ApiState,
    started: Instant,
}

fn anthropic_stream(
    state: ApiState,
    prep: Prepared,
    upstream: UpstreamStream,
    permit: GatePermit,
    started: Instant,
) -> Response {
    let id = msg_id();
    let mut pending = VecDeque::new();
    pending.push_back(message_start(&id, &prep.model));
    let ctx = StreamCtx {
        up: upstream,
        gate: TextGate::new(prep.tools_enabled()),
        _permit: permit,
        model: prep.model.clone(),
        pending,
        next_index: 0,
        text_open: false,
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
                    // 流已开始：错误事件帧后关闭（Anthropic 错误帧为 {"type":"error",...}）。
                    ctx.pending.push_back(frame("error", e.anthropic_body()));
                    ctx.ended = true;
                }
                Some(Ok(msg)) => match msg {
                    StreamMsg::Text(t) => {
                        if !ctx.text_open {
                            ctx.pending.push_back(text_start(ctx.next_index));
                            ctx.next_index += 1;
                            ctx.text_open = true;
                        }
                        let idx = ctx.next_index - 1;
                        ctx.pending.push_back(text_delta(idx, &t));
                    }
                    StreamMsg::Reasoning(_) => {}
                    StreamMsg::ToolCall {
                        name, arguments, ..
                    } => {
                        if ctx.text_open {
                            ctx.pending.push_back(block_stop(ctx.next_index - 1));
                            ctx.text_open = false;
                        }
                        let idx = ctx.next_index;
                        ctx.next_index += 1;
                        let tid = format!("toolu_{}", hex24());
                        ctx.pending.push_back(tool_start(idx, &tid, &name));
                        ctx.pending
                            .push_back(tool_delta(idx, &arguments.to_string()));
                        ctx.pending.push_back(block_stop(idx));
                    }
                    StreamMsg::Ping => {
                        ctx.pending
                            .push_back(frame("ping", json!({ "type": "ping" })));
                    }
                    StreamMsg::End(end) => {
                        let mut stop = end.reason.as_str();
                        if !end.tail.is_empty() {
                            if !ctx.text_open {
                                ctx.pending.push_back(text_start(ctx.next_index));
                                ctx.next_index += 1;
                                ctx.text_open = true;
                            }
                            let idx = ctx.next_index - 1;
                            ctx.pending.push_back(text_delta(idx, &end.tail));
                        }
                        let envelope = end.tool;
                        if let Some(env) = &envelope {
                            if ctx.text_open {
                                ctx.pending.push_back(block_stop(ctx.next_index - 1));
                                ctx.text_open = false;
                            }
                            let idx = ctx.next_index;
                            ctx.next_index += 1;
                            let tid = format!("toolu_{}", hex24());
                            ctx.pending.push_back(tool_start(idx, &tid, &env.name));
                            let args = Value::Object(env.input.clone());
                            ctx.pending.push_back(tool_delta(idx, &args.to_string()));
                            ctx.pending.push_back(block_stop(idx));
                        }
                        if ctx.text_open {
                            ctx.pending.push_back(block_stop(ctx.next_index - 1));
                            ctx.text_open = false;
                        }
                        if envelope.is_some() || stop == "tool_calls" {
                            stop = "tool_use";
                        } else if stop == "length" {
                            stop = "max_tokens";
                        } else if stop != "end_turn" {
                            stop = "end_turn";
                        }
                        ctx.pending.push_back(message_delta(stop));
                        ctx.pending.push_back(message_stop());
                        log_finish(&ctx.state, PROTO_NAME, &ctx.model, ctx.started, 200, None);
                        ctx.ended = true;
                    }
                },
            }
        }
    }))
}
