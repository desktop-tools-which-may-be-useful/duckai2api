//! 三协议公共请求预备：JSON 解析 → 模型本地解析（P1-8 404）→ 扁平化 →
//! 工具意图路由（P0-2 修复位）→ 信封注入 → 上游请求装配。

use crate::gate::GatePermit;
use duckai_protocol::flatten::anthropic::AnthropicInput;
use duckai_protocol::flatten::openai::ChatCompletionsInput;
use duckai_protocol::flatten::responses::ResponsesInput;
use duckai_protocol::flatten::{flatten_anthropic, flatten_openai, flatten_responses};
use duckai_protocol::{ToolCallEnvelope, render_tools_prompt, route_from_turns};
use duckai_types::model::ModelCatalog;
use duckai_types::{
    ChatTurn, ContentBlock, LogEntry, Role, ToolChoice, ToolDef, TurnContent, UpstreamRequest,
};
use serde_json::Value;

use crate::ApiState;
use crate::error::{ApiErr, Proto};
use crate::logs::now_ms;
use crate::stream::hex24;
use duckai_upstream::UpstreamStream;

/// 预备完毕的一次对话请求。
#[derive(Debug, Clone)]
pub struct Prepared {
    /// 目录解析后的规范模型名。
    pub model: String,
    pub turns: Vec<ChatTurn>,
    pub tools: Vec<ToolDef>,
    pub tool_choice: Option<ToolChoice>,
    /// 话术直连路由命中的工具调用（不打上游，直接合成响应）。
    pub routed: Option<ToolCallEnvelope>,
    pub stream: bool,
    pub session_hint: Option<String>,
    pub reasoning_effort: Option<String>,
}

impl Prepared {
    pub fn tools_enabled(&self) -> bool {
        !self.tools.is_empty()
    }

    pub fn to_upstream(&self) -> UpstreamRequest {
        let mut req = UpstreamRequest::new(self.model.clone(), self.turns.clone());
        req.tools = self.tools.clone();
        req.tool_choice = self.tool_choice.clone();
        req.reasoning_effort = self.reasoning_effort.clone();
        req.session_hint = self.session_hint.clone();
        req
    }

    /// 路由合成的工具调用 → `(name, arguments)`。
    pub fn routed_tool(&self) -> Option<(String, Value)> {
        self.routed
            .as_ref()
            .map(|c| (c.name.clone(), Value::Object(c.input.clone())))
    }
}

/// 模型本地解析：上游目录（30min 缓存）→ 未知则快照 → 仍未知 404（不透传上游）。
async fn resolve_model(state: &ApiState, requested: &str) -> Result<String, ApiErr> {
    let models = match state.upstream.list_models().await {
        Ok(v) if !v.is_empty() => v,
        _ => ModelCatalog::snapshot().list().to_vec(),
    };
    let catalog = ModelCatalog::from_upstream(models);
    match catalog.resolve(requested) {
        Some(info) => Ok(info.id.clone()),
        None => Err(ApiErr::ModelNotFound(requested.to_string())),
    }
}

/// 同会话粘性线索：`user` / `metadata.user_id` / `metadata.session_hint`。
fn session_hint_of(raw: &Value) -> Option<String> {
    let direct = raw
        .get("user")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string);
    let meta = raw
        .get("metadata")
        .and_then(|m| m.get("user_id").or_else(|| m.get("session_hint")))
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string);
    direct.or(meta)
}

fn reasoning_effort_of(raw: &Value) -> Option<String> {
    raw.get("reasoning_effort")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| {
            raw.get("reasoning")
                .and_then(|r| r.get("effort"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
}

/// 预备一次请求（协议族决定解析器；错误帧由调用方按协议渲染）。
pub async fn prepare(state: &ApiState, mut raw: Value, proto: Proto) -> Result<Prepared, ApiErr> {
    let stream = raw.get("stream").and_then(Value::as_bool).unwrap_or(false);

    // OpenAI 入参 model 必填：缺省回落 DUCKAI_MODEL（消费点）。
    if !raw.get("model").is_some_and(Value::is_string) {
        raw["model"] = Value::String(state.default_model());
    }
    // 元数据先于解析抽取（raw 随后按协议族被移动）。
    let session_hint = session_hint_of(&raw);
    let reasoning_effort = reasoning_effort_of(&raw);

    let (turns, tools, tool_choice, model) = match proto {
        Proto::OpenAi => {
            let input: ChatCompletionsInput =
                serde_json::from_value(raw).map_err(|e| ApiErr::Invalid(e.to_string()))?;
            let flat = flatten_openai(&input).map_err(|e| ApiErr::Invalid(e.to_string()))?;
            (flat.turns, flat.tools, flat.tool_choice, input.model)
        }
        Proto::Anthropic => {
            let input: AnthropicInput =
                serde_json::from_value(raw).map_err(|e| ApiErr::Invalid(e.to_string()))?;
            let flat = flatten_anthropic(&input).map_err(|e| ApiErr::Invalid(e.to_string()))?;
            let model = input.model.unwrap_or_else(|| state.default_model());
            (flat.turns, flat.tools, flat.tool_choice, model)
        }
        Proto::Responses => {
            let input: ResponsesInput =
                serde_json::from_value(raw).map_err(|e| ApiErr::Invalid(e.to_string()))?;
            let flat = flatten_responses(&input).map_err(|e| ApiErr::Invalid(e.to_string()))?;
            let model = input.model.unwrap_or_else(|| state.default_model());
            (flat.turns, flat.tools, flat.tool_choice, model)
        }
    };

    // 空提示词：无 tools 时 400（原项目语义）；有 tools 时允许（路由可能合成）。
    let has_text = turns.iter().any(|t| !t.content.text().trim().is_empty());
    if !has_text && tools.is_empty() {
        return Err(ApiErr::Invalid("无效的请求：messages 内容不能为空".into()));
    }

    let model = resolve_model(state, &model).await?;

    // 话术直连路由：tools 存在且无 tool_result 轮 → 合成工具调用（P0-2 修复位）。
    let routed = if tools.is_empty() {
        None
    } else {
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        route_from_turns(&turns, &names)
    };

    let mut turns = turns;
    if routed.is_none() && !tools.is_empty() {
        inject_envelope(&mut turns, &tools);
    }

    Ok(Prepared {
        model,
        turns,
        tools,
        tool_choice,
        routed,
        stream,
        session_hint,
        reasoning_effort,
    })
}

/// 信封注入：把 `render_tools_prompt` 教学后缀拼到最后一条用户轮（ARCHITECTURE 要求
/// 「完整接入信封注入 + 解析」；原 Python 的 `render_tools_prompt` 是零调用僵尸）。
fn inject_envelope(turns: &mut Vec<ChatTurn>, tools: &[ToolDef]) {
    let suffix = render_tools_prompt(tools);
    if suffix.is_empty() {
        return;
    }
    if let Some(i) = turns.iter().rposition(|t| t.role == Role::User) {
        match &mut turns[i].content {
            TurnContent::Text(t) => t.push_str(&suffix),
            TurnContent::Blocks(blocks) => blocks.push(ContentBlock::Text { text: suffix }),
        }
    } else {
        // 没有用户轮（纯 tool_result 等）→ 追加一条用户轮承载教学。
        turns.push(ChatTurn::text(Role::User, suffix));
    }
}

/// 路由合成响应的工具调用 id（按协议族风格）。
pub fn routed_call_id(proto: Proto) -> String {
    match proto {
        Proto::OpenAi => format!("call_{}", hex24()),
        Proto::Anthropic => format!("toolu_{}", hex24()),
        Proto::Responses => format!("fc_{}", hex24()),
    }
}

/// 开闸 + 打上游：取不到令牌 → `Busy`（API 层 429，不排队）；
/// 上游调用失败则令牌随作用域立即归还。
pub async fn start_chat(
    state: &ApiState,
    prep: &Prepared,
) -> Result<(UpstreamStream, GatePermit), ApiErr> {
    let permit = state
        .gate
        .try_acquire()
        .ok_or(ApiErr::Busy(crate::resp::BUSY_RETRY_AFTER))?;
    match state.upstream.chat(prep.to_upstream()).await {
        Ok(s) => Ok((s, permit)),
        Err(e) => Err(ApiErr::from(e)),
    }
}

/// 请求结束记账（含流式：调用方在流终止时补记）。
pub fn log_finish(
    state: &ApiState,
    protocol: &str,
    model: &str,
    started: std::time::Instant,
    status: u16,
    error: Option<String>,
) {
    state.logs.push(LogEntry {
        ts_ms: now_ms(),
        protocol: protocol.to_string(),
        model: model.to_string(),
        duration_ms: started.elapsed().as_millis() as u64,
        status,
        retries: 0,
        switch_reason: None,
        error,
    });
}
