//! Duck.ai `/duckchat/v1/chat` 请求体构造（§2.2.6）。
//!
//! 字段与线上验证过的 wire 形状逐一对齐（真实 200 SSE 实测）：
//! `model, messages, canUseTools, reasoningEffort, canUseApproxLocation,
//! canDelegateImageGeneration, canShowGreeting, durableStream`。
//! `durableStream.publicKey` 为 RSA-OAEP-256 JWK（上游以该钥加密回流密文，我们不持有私钥）；
//! `conversationId` 按 `session_hint` 粘性复用（多轮上下文），`DUCKAI_NEW_CHAT=true` 时每次新开。

use std::collections::HashMap;
use std::sync::Mutex;

use base64::Engine as _;
use rand::Rng as _;
use serde::Serialize;
use sha2::{Digest as _, Sha256};

use duckai_types::{ChatTurn, Role, ToolChoice, UpstreamRequest};

/// 上游接受的推理档位默认值。
pub const REASONING_DEFAULT: &str = "none";

/// 请求体顶层。
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DuckChatBody {
    pub model: String,
    pub messages: Vec<DuckMessage>,
    pub can_use_tools: bool,
    pub reasoning_effort: String,
    /// 原生内建工具开关（aurora 参考实现恒发送；false 档见 `ToolChoice::None`）。
    pub metadata: DuckMetadata,
    /// 线上恒为 null（地理授权位，纯 HTTP 路径不申请）。
    pub can_use_approx_location: Option<bool>,
    pub can_delegate_image_generation: Option<bool>,
    pub can_show_greeting: bool,
    pub durable_stream: DurableStream,
}

/// 内建工具选择（wire 形状与 aurora 参考一致：GenerateImage/WebSearch 为 true 时才出现）。
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DuckMetadata {
    pub tool_choice: DuckBuiltinToolChoice,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DuckBuiltinToolChoice {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generate_image: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub web_search: Option<bool>,
    pub news_search: bool,
    pub videos_search: bool,
    pub local_search: bool,
    pub weather_forecast: bool,
}

/// 一条消息：user 为 parts 数组，assistant 为 `content:""` + `parts` 回放。
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct DuckMessage {
    pub role: &'static str,
    pub content: MessageContent,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parts: Option<Vec<TextPart>>,
}

/// text part（字段序 `type,text` 与线上一致）。
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TextPart {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub text: String,
}

/// user：`[{"type":"text","text":...}]`；assistant：`""`。
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(untagged)]
pub enum MessageContent {
    Blocks(Vec<TextPart>),
    Plain(String),
}

impl DuckMessage {
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: "user",
            content: MessageContent::Blocks(vec![TextPart {
                kind: "text",
                text: text.into(),
            }]),
            parts: None,
        }
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: "assistant",
            content: MessageContent::Plain(String::new()),
            parts: Some(vec![TextPart {
                kind: "text",
                text: text.into(),
            }]),
        }
    }
}

/// durableStream 段。
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DurableStream {
    pub message_id: String,
    pub conversation_id: String,
    pub public_key: RsaJwk,
}

/// RSA-OAEP-256 公钥 JWK（字段序 alg,e,ext,key_ops,kty,n,use 与线上一致）。
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RsaJwk {
    pub alg: String,
    pub e: String,
    pub ext: bool,
    pub key_ops: Vec<String>,
    pub kty: String,
    pub n: String,
    #[serde(rename = "use")]
    pub usage: String,
}

impl RsaJwk {
    /// 公钥参数。
    pub fn public_only(n: String, e: String) -> Self {
        Self {
            alg: "RSA-OAEP-256".into(),
            e,
            ext: true,
            key_ops: vec!["encrypt".into()],
            kty: "RSA".into(),
            n,
            usage: "enc".into(),
        }
    }

    /// 生成 2048-bit RSA 公钥 JWK（上游用它做 RSA-OAEP-256 加密回流；我们不持私钥）。
    pub fn generate() -> Result<Self, JwkError> {
        use rsa::traits::PublicKeyParts as _;

        // rsa 0.9 需要 rand_core 0.6 的 CryptoRng —— 用 rand 0.8 的 OsRng（见 Cargo.toml 注释）
        let mut rng = rand08::rngs::OsRng;
        let key = rsa::RsaPrivateKey::new(&mut rng, 2048).map_err(|e| JwkError(e.to_string()))?;
        let comps = key.to_public_key();
        let n = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(comps.n().to_bytes_be());
        let e = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(comps.e().to_bytes_be());
        Ok(Self::public_only(n, e))
    }
}

#[derive(Debug, thiserror::Error)]
#[error("rsa keygen failed: {0}")]
pub struct JwkError(String);

/// 生成 durableStream 用的加密公钥 JWK。
pub fn generate_encryption_jwk() -> Result<RsaJwk, JwkError> {
    RsaJwk::generate()
}

/// 会话粘性：`session_hint → conversationId` 复用（`DUCKAI_NEW_CHAT=true` 时禁用）。
#[derive(Debug, Default)]
pub struct StickyConversation {
    map: Mutex<HashMap<String, String>>,
}

impl StickyConversation {
    pub fn new() -> Self {
        Self::default()
    }

    /// 取（或新建）会话 id。`new_chat` 为 true 时总是新开会话。
    pub fn conversation_id_for(&self, session_hint: Option<&str>, new_chat: bool) -> String {
        if new_chat {
            return uuid_v4();
        }
        let Some(hint) = session_hint else {
            return uuid_v4();
        };
        let mut map = match self.map.lock() {
            Ok(m) => m,
            Err(_) => return uuid_v4(),
        };
        map.entry(hint.to_string()).or_insert_with(uuid_v4).clone()
    }
}

/// 随机 UUIDv4 文本（小写 hex，RFC 4122 变体位）。
pub fn uuid_v4() -> String {
    let mut bytes = [0u8; 16];
    rand::rng().fill(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// 轮次 → duck 消息映射（§1.3 角色标签保序策略的 wire 落点）。
pub fn to_duck_messages(turns: &[ChatTurn]) -> Vec<DuckMessage> {
    turns
        .iter()
        .map(|t| match t.role {
            Role::Assistant => DuckMessage::assistant(t.content.text()),
            Role::System => DuckMessage::user(format!("[System]: {}", t.content.text())),
            Role::Tool => {
                let label = match &t.tool_call_id {
                    Some(id) => format!("[tool_result {id}]: "),
                    None => "[tool_result]: ".to_string(),
                };
                DuckMessage::user(format!("{label}{}", t.content.text()))
            }
            Role::User => DuckMessage::user(t.content.text()),
        })
        .collect()
}

/// 构造一次发问的完整请求体。
///
/// * `new_chat` —— `DUCKAI_NEW_CHAT`（true 时 `conversationId` 每次新开，不再粘性）；
/// * `tool_choice=Some(ToolChoice::None)` —— 客户端显式禁用工具 → `canUseTools:false`（否则恒 true，与参考实现一致）。
pub fn build_chat_body(
    req: &UpstreamRequest,
    sticky: &StickyConversation,
    jwk: &RsaJwk,
    new_chat: bool,
) -> DuckChatBody {
    let can_use_tools = !matches!(req.tool_choice, Some(ToolChoice::None));
    DuckChatBody {
        model: req.model.clone(),
        messages: to_duck_messages(&req.turns),
        can_use_tools,
        reasoning_effort: req
            .reasoning_effort
            .clone()
            .unwrap_or_else(|| REASONING_DEFAULT.to_string()),
        metadata: DuckMetadata {
            tool_choice: DuckBuiltinToolChoice::default(),
        },
        can_use_approx_location: None,
        can_delegate_image_generation: None,
        can_show_greeting: true,
        durable_stream: DurableStream {
            message_id: uuid_v4(),
            conversation_id: sticky.conversation_id_for(req.session_hint.as_deref(), new_chat),
            public_key: jwk.clone(),
        },
    }
}

/// 便捷入口：无粘性表的一次性构造（测试/CLI）。
pub fn build_chat_body_once(req: &UpstreamRequest, jwk: &RsaJwk, new_chat: bool) -> DuckChatBody {
    build_chat_body(req, &StickyConversation::new(), jwk, new_chat)
}

/// 从会话推导请求指纹（上游会话粘性选路用的非隐私摘要）。
pub fn session_fingerprint(session_hint: &str) -> String {
    let digest = Sha256::digest(session_hint.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&digest[..8])
}

#[cfg(test)]
mod tests {
    use super::*;
    use duckai_types::{ChatTurn, Role};

    fn req() -> UpstreamRequest {
        UpstreamRequest::new(
            "gpt-5.6-luna",
            vec![
                ChatTurn::text(Role::User, "Say exactly: OK and stop."),
                ChatTurn::text(Role::Assistant, "OK"),
                ChatTurn::text(Role::User, "Say exactly: PONG and stop."),
            ],
        )
    }

    #[test]
    fn body_wire_shape_matches_live_capture() {
        let jwk = RsaJwk::public_only("NN".into(), "AQAB".into());
        let body = build_chat_body(&req(), &StickyConversation::new(), &jwk, false);
        let v = serde_json::to_value(&body).unwrap();
        assert_eq!(v["model"], "gpt-5.6-luna");
        assert_eq!(v["messages"][0]["role"], "user");
        assert_eq!(
            v["messages"][0]["content"],
            serde_json::json!([{"type": "text", "text": "Say exactly: OK and stop."}])
        );
        assert_eq!(v["messages"][1]["role"], "assistant");
        assert_eq!(v["messages"][1]["content"], "");
        assert_eq!(
            v["messages"][1]["parts"],
            serde_json::json!([{"type": "text", "text": "OK"}])
        );
        assert_eq!(v["canUseTools"], true);
        assert_eq!(v["reasoningEffort"], "none");
        assert_eq!(v["canUseApproxLocation"], serde_json::Value::Null);
        assert_eq!(v["canDelegateImageGeneration"], serde_json::Value::Null);
        assert_eq!(v["canShowGreeting"], true);
        assert_eq!(v["durableStream"]["publicKey"]["alg"], "RSA-OAEP-256");
        assert_eq!(v["durableStream"]["publicKey"]["e"], "AQAB");
        assert_eq!(
            v["metadata"]["toolChoice"],
            serde_json::json!({
                "newsSearch": false, "videosSearch": false,
                "localSearch": false, "weatherForecast": false,
            })
        );
    }

    #[test]
    fn key_order_in_message_parts_is_type_then_text() {
        let jwk = RsaJwk::public_only("NN".into(), "AQAB".into());
        let body = build_chat_body(&req(), &StickyConversation::new(), &jwk, false);
        let raw = serde_json::to_string(&body).unwrap();
        let idx_type = raw.find("\"type\"").unwrap();
        let idx_text = raw.find("\"text\"").unwrap();
        assert!(idx_type < idx_text, "part 字段序必须 type 在前: {raw}");
    }

    #[test]
    fn sticky_conversation_reuses_and_new_chat_forces_new() {
        let sticky = StickyConversation::new();
        let a = sticky.conversation_id_for(Some("s1"), false);
        let b = sticky.conversation_id_for(Some("s1"), false);
        assert_eq!(a, b, "同 session_hint 复用 conversationId");
        let c = sticky.conversation_id_for(Some("s1"), true);
        assert_ne!(a, c, "DUCKAI_NEW_CHAT=true 强制新开");
        let d = sticky.conversation_id_for(None, false);
        assert_ne!(a, d, "无 hint 不能粘住");
        let e = sticky.conversation_id_for(Some("s2"), false);
        assert_ne!(a, e, "不同 hint 互不串会话");
    }

    #[test]
    fn tool_choice_none_disables_can_use_tools() {
        let jwk = RsaJwk::public_only("NN".into(), "AQAB".into());
        let mut r = req();
        r.tool_choice = Some(ToolChoice::None);
        let body = build_chat_body(&r, &StickyConversation::new(), &jwk, false);
        assert!(!body.can_use_tools);
        r.tool_choice = Some(ToolChoice::Auto);
        assert!(build_chat_body(&r, &StickyConversation::new(), &jwk, false).can_use_tools);
        r.tool_choice = None;
        assert!(build_chat_body(&r, &StickyConversation::new(), &jwk, false).can_use_tools);
    }

    #[test]
    fn uuid_shape() {
        let id = uuid_v4();
        assert_eq!(id.len(), 36);
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12]
        );
        assert_eq!(parts[2].chars().next().unwrap(), '4', "version nibble");
        assert!(matches!(
            parts[3].chars().next().unwrap(),
            '8' | '9' | 'a' | 'b'
        ));
        assert_ne!(uuid_v4(), uuid_v4());
    }

    #[test]
    fn reasoning_and_system_tool_labels() {
        let jwk = RsaJwk::public_only("NN".into(), "AQAB".into());
        let r = UpstreamRequest {
            model: "m".into(),
            turns: vec![
                ChatTurn::text(Role::System, "be brief"),
                ChatTurn::text(Role::User, "hi"),
                {
                    let mut t = ChatTurn::text(Role::Tool, "tool output");
                    t.tool_call_id = Some("call_1".into());
                    t
                },
                ChatTurn::text(Role::User, "continue"),
            ],
            tool_choice: None,
            tools: vec![],
            reasoning_effort: Some("high".into()),
            session_hint: None,
        };
        let body = build_chat_body(&r, &StickyConversation::new(), &jwk, false);
        assert_eq!(body.reasoning_effort, "high");
        assert_eq!(
            body.messages[0].content,
            MessageContent::Blocks(vec![TextPart {
                kind: "text",
                text: "[System]: be brief".into()
            }])
        );
        match &body.messages[2].content {
            MessageContent::Blocks(parts) => {
                assert_eq!(parts[0].text, "[tool_result call_1]: tool output")
            }
            other => panic!("unexpected content: {other:?}"),
        }
    }

    #[test]
    fn generated_jwk_is_2048_bit_urlsafe() {
        let jwk = RsaJwk::generate().expect("keygen");
        assert_eq!(jwk.alg, "RSA-OAEP-256");
        assert_eq!(jwk.e, "AQAB");
        assert_eq!(jwk.n.len(), 342, "2048-bit moduli base64url len");
        assert!(
            jwk.n
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
        let json = serde_json::to_string(&jwk).unwrap();
        assert!(!json.contains("private"), "私钥不得进 wire");
        assert!(json.contains("\"use\":\"enc\""));
    }
}
