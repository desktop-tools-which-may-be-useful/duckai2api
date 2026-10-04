//! `POST /v1/chat/completions`（OpenAI 形状：非流式 + SSE 流式 + 工具编排）。

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
use crate::prep::{Prepared, log_finish, prepare, routed_call_id, start_chat};
use crate::resp::{error_response, json_response, sse_response};
use crate::stream::{Collected, TextGate, collect, completion_id, pump};

const PROTO_NAME: &str = "chat";

pub async fn chat(State(state): State<ApiState>, body: Bytes) -> Response {
    let started = Instant::now();
    let proto = Proto::OpenAi;

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

    // 话术直连路由：合成工具调用，不打上游、不过闸门（P0-2）。
    if let Some(col) = routed_collected(proto, &prep) {
        log_finish(&state, PROTO_NAME, &prep.model, started, 200, None);
        return if prep.stream {
            openai_stream_response(&prep.model, col)
        } else {
            json_response(StatusCode::OK, completion_json(&prep.model, &col))
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
        openai_stream(state, prep, upstream, permit, started)
    } else {
        openai_nonstream(state, prep, upstream, permit, started).await
    }
}

/// prepare 阶段失败时日志里没有模型名 → 用默认值（404 时记客户端请求的名字）。
pub(crate) fn prep_model_fallback(state: &ApiState, e: &ApiErr) -> String {
    match e {
        ApiErr::ModelNotFound(m) => m.clone(),
        _ => state.default_model(),
    }
}

/// 路由命中 → 直接合成的非流式收集结果（不打上游）。
pub(crate) fn routed_collected(proto: Proto, prep: &Prepared) -> Option<Collected> {
    let (name, args) = prep.routed_tool()?;
    Some(Collected {
        text: String::new(),
        tool_event: Some((routed_call_id(proto), name, args)),
        tool_envelope: None,
        reason: "tool_calls".into(),
    })
}

fn completion_json(model: &str, col: &Collected) -> Value {
    let (content, tool_calls, finish) = match col.tool() {
        Some((tid, name, args)) => {
            let calls = json!([{
                "id": tid,
                "type": "function",
                "function": { "name": name, "arguments": args.to_string() },
            }]);
            let pre = col.text.trim();
            let content = if pre.is_empty() {
                Value::Null
            } else {
                Value::String(pre.to_string())
            };
            (content, Some(calls), "tool_calls")
        }
        None => {
            let reason = if col.reason.is_empty() {
                "stop"
            } else {
                col.reason.as_str()
            };
            (Value::String(col.text.clone()), None, reason)
        }
    };
    let mut message = json!({ "role": "assistant", "content": content });
    if let Some(calls) = tool_calls {
        message["tool_calls"] = calls;
    }
    json!({
        "id": completion_id(),
        "object": "chat.completion",
        "created": now_secs(),
        "model": model,
        "choices": [{ "index": 0, "message": message, "finish_reason": finish }],
        "usage": { "prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0 },
    })
}

async fn openai_nonstream(
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
            return error_response(Proto::OpenAi, &e);
        }
    };
    log_finish(&state, PROTO_NAME, &prep.model, started, 200, None);
    json_response(StatusCode::OK, completion_json(&prep.model, &col))
}

/// 路由合成流（不打上游）：同一条流内 `chatcmpl-` id 一致。
pub(crate) fn openai_stream_response(model: &str, col: Collected) -> Response {
    let id = completion_id();
    let created = now_secs();
    let mut frames = VecDeque::new();
    frames.push_back(role_frame(&id, model, created));
    if let Some((tid, name, args)) = col.tool() {
        frames.push_back(tool_frame(&id, model, created, &tid, &name, &args));
    } else if !col.text.is_empty() {
        frames.push_back(text_frame(&id, model, created, &col.text));
    }
    frames.push_back(finish_frame(
        &id,
        model,
        created,
        if col.tool().is_some() {
            "tool_calls"
        } else {
            "stop"
        },
    ));
    frames.push_back("data: [DONE]\n\n".to_string());
    sse_response(stream::iter(frames))
}

fn role_frame(id: &str, model: &str, created: u64) -> String {
    chunk_frame(
        id,
        model,
        created,
        json!({ "role": "assistant", "content": "" }),
        Value::Null,
    )
}

fn text_frame(id: &str, model: &str, created: u64, text: &str) -> String {
    chunk_frame(id, model, created, json!({ "content": text }), Value::Null)
}

fn tool_frame(id: &str, model: &str, created: u64, tid: &str, name: &str, args: &Value) -> String {
    chunk_frame(
        id,
        model,
        created,
        json!({
            "tool_calls": [{
                "index": 0,
                "id": tid,
                "type": "function",
                "function": { "name": name, "arguments": args.to_string() },
            }],
        }),
        Value::Null,
    )
}

fn finish_frame(id: &str, model: &str, created: u64, finish: &str) -> String {
    chunk_frame(id, model, created, json!({}), json!(finish))
}

fn chunk_frame(id: &str, model: &str, created: u64, delta: Value, finish: Value) -> String {
    format!(
        "data: {}\n\n",
        json!({
            "id": id,
            "object": "chat.completion.chunk",
            "created": created,
            "model": model,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
        })
    )
}

struct StreamCtx {
    up: UpstreamStream,
    gate: TextGate,
    _permit: GatePermit,
    id: String,
    model: String,
    created: u64,
    pending: VecDeque<String>,
    ended: bool,
    state: ApiState,
    started: Instant,
}

fn openai_stream(
    state: ApiState,
    prep: Prepared,
    upstream: UpstreamStream,
    permit: GatePermit,
    started: Instant,
) -> Response {
    let id = completion_id();
    let created = now_secs();
    let mut pending = VecDeque::new();
    pending.push_back(role_frame(&id, &prep.model, created));
    let ctx = StreamCtx {
        up: upstream,
        gate: TextGate::new(prep.tools_enabled()),
        _permit: permit,
        id,
        model: prep.model.clone(),
        created,
        pending,
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
                None => Some(Ok(crate::stream::StreamMsg::End(ctx.gate.end("stop")))),
                other => other,
            };
            match msg {
                None => unreachable!("pump 的 None 已在上方折叠"),
                Some(Err(e)) => {
                    // 流内错误：错误帧 + [DONE]（HTTP 头已发，状态码体现在帧里并记日志）。
                    log_finish(
                        &ctx.state,
                        PROTO_NAME,
                        &ctx.model,
                        ctx.started,
                        e.status().as_u16(),
                        Some(e.code()),
                    );
                    ctx.ended = true;
                    ctx.pending
                        .push_back(format!("data: {}\n\n", e.openai_body()));
                    ctx.pending.push_back("data: [DONE]\n\n".into());
                }
                Some(Ok(msg)) => match msg {
                    crate::stream::StreamMsg::Text(t) => {
                        ctx.pending
                            .push_back(text_frame(&ctx.id, &ctx.model, ctx.created, &t));
                    }
                    crate::stream::StreamMsg::Reasoning(r) => {
                        ctx.pending.push_back(chunk_frame(
                            &ctx.id,
                            &ctx.model,
                            ctx.created,
                            json!({ "reasoning_content": r }),
                            Value::Null,
                        ));
                    }
                    crate::stream::StreamMsg::ToolCall {
                        id: tid,
                        name,
                        arguments,
                    } => {
                        ctx.pending.push_back(tool_frame(
                            &ctx.id,
                            &ctx.model,
                            ctx.created,
                            &tid,
                            &name,
                            &arguments,
                        ));
                    }
                    crate::stream::StreamMsg::Ping => continue,
                    crate::stream::StreamMsg::End(end) => {
                        if !end.tail.is_empty() {
                            ctx.pending.push_back(text_frame(
                                &ctx.id,
                                &ctx.model,
                                ctx.created,
                                &end.tail,
                            ));
                        }
                        let has_tool = end.tool.is_some();
                        if let Some(env) = &end.tool {
                            let args = Value::Object(env.input.clone());
                            let tid = crate::stream::hex24();
                            ctx.pending.push_back(tool_frame(
                                &ctx.id,
                                &ctx.model,
                                ctx.created,
                                &format!("call_{tid}"),
                                &env.name,
                                &args,
                            ));
                        }
                        let finish: &str = if has_tool {
                            "tool_calls"
                        } else {
                            end.reason.as_str()
                        };
                        ctx.pending.push_back(finish_frame(
                            &ctx.id,
                            &ctx.model,
                            ctx.created,
                            finish,
                        ));
                        ctx.pending.push_back("data: [DONE]\n\n".into());
                        log_finish(&ctx.state, PROTO_NAME, &ctx.model, ctx.started, 200, None);
                        ctx.ended = true;
                    }
                },
            }
        }
    }))
}
