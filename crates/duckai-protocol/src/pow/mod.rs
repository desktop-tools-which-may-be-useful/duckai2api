//! 挑战求解（Proof-of-Work / VQD token 生成）。
//!
//! Duck.ai 的 `X-Vqd-Hash-1` 请求头由「服务端挑战 → 客户端求解 → mutation → base64」四步产生。
//! 本模块把这条链做成可替换的 [`PowEngine`]：
//!
//! - [`RquickjsPow`]：默认引擎，在 QuickJS 里执行挑战 JS（ARCHITECTURE.md §5.2 本地求解层）；
//! - 浏览器引擎（`browser` feature，M5 可选）：同一 trait 的另一实现；
//! - `DUCKAI_VQD_OVERRIDE` 覆盖令牌在 `VqdStore` 中先行短路（§5.4 的第一优先级之外的旁路）。
//!
//! 求解语义与 aurora-develop/Duck2api 的 `vqd.go` 一致（含错误回退串格式），
//! 且已用**真实线上挑战**端到端验证（本仓库 fixtures/challenge_fresh.b64 可重放）。

use base64::Engine as _;
use rand::Rng as _;

pub mod rquickjs_engine;

pub use rquickjs_engine::RquickjsPow;

/// 内嵌资产（改编自 aurora-develop/Duck2api，MIT）。
pub const PRELUDE_JS: &str = include_str!("prelude.js");
/// 求解结果 mutation 脚本（保留 server_hashes/signals 顺序，对 client_hashes 做 SHA-256→base64）。
pub const MUTATION_JS: &str = include_str!("mutation.js");

/// 求解所需的环境输入（由上游接入层从真实首页/配置推导，见 [`crate::home`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PowEnv {
    /// 浏览器 User-Agent（同时决定 `client_hashes[0] = SHA256(UA)`）。
    pub user_agent: String,
    /// 页面 origin，例如 `https://duck.ai`。
    pub origin: String,
    /// meta.stack：`entry.duckai.<hash>.js` 调用栈（格式见 `crate::home::stack_for_bundle`）。
    pub stack: String,
}

/// 求解产物。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PowSolution {
    /// `X-Vqd-Hash-1` 请求头值：base64(JSON)。
    pub payload_b64: String,
    /// mutation 前的原始 client_hashes（诊断用，首个恒为 `SHA256(UA)`）。
    pub raw_client_hashes: Vec<String>,
    /// 服务端挑战 id（`meta.challenge_id`，若挑战未提供则为 `None`）。
    pub challenge_id: Option<String>,
    /// 求解耗时（毫秒，mutation 写入 `meta.duration` 的字符串形式）。
    pub duration_ms: String,
}

/// 求解错误。
#[derive(Debug, thiserror::Error)]
pub enum PowError {
    #[error("challenge base64 decode failed: {0}")]
    Decode(String),
    #[error("browser mock init failed: {0}")]
    Prelude(String),
    #[error("challenge evaluation failed: {0}")]
    Eval(String),
    #[error("challenge did not settle: {0}")]
    Unsettled(String),
    #[error("challenge execution timed out")]
    Timeout,
    #[error("result mutation failed: {0}")]
    Mutation(String),
    #[error("pow engine unavailable")]
    Unavailable,
}

/// PoW 引擎抽象（§5.4 求解链的可插拔接口）。
///
/// `challenge_b64` 是 `X-Vqd-Hash-1` 响应头里的服务端挑战（base64 编码的 JS 表达式）。
pub trait PowEngine: Send + Sync + std::fmt::Debug {
    fn solve(&self, challenge_b64: &str, env: &PowEnv) -> Result<PowSolution, PowError>;
}

/// 标准 base64（与 Go `base64.StdEncoding` 对齐）。
pub fn b64_decode(input: &str) -> Result<Vec<u8>, PowError> {
    base64::engine::general_purpose::STANDARD
        .decode(input.trim())
        .map_err(|e| PowError::Decode(e.to_string()))
}

/// 标准 base64 编码字节串。
pub fn b64_encode(input: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(input)
}

/// `SHA-256 → base64`（等价于线上 `__goSha256Base64` / `crypto.subtle.digest` 链）。
pub fn sha256_base64(input: &str) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(input.as_bytes());
    b64_encode(&digest)
}

/// 浏览器形态求解载荷的 `meta.duration` 对齐。
///
/// 线上两次成功 chat 的 duration 分别为 16ms（duck.ai.har）与 12ms（自然流程），
/// 都是真实浏览器页内求解耗时；本地 QuickJS 求解 ~0–1ms 直接上报与 Chrome 形态
/// 不符（服务端对 Chrome 形态请求做严格校验，见浏览器模式 418 根因分析）。
/// 本函数把 `meta.duration` 改写为 8–20ms 的观测区间（不触碰任何哈希字段：
/// duration 不参与 `client_hashes` 计算，载荷无签名）。解析失败原样返回。
pub fn align_duration(payload_b64: &str) -> String {
    let Ok(raw) = b64_decode(payload_b64) else {
        return payload_b64.to_string();
    };
    let Ok(mut val) = serde_json::from_slice::<serde_json::Value>(&raw) else {
        return payload_b64.to_string();
    };
    let ms = 8 + rand::rng().random_range(0..=12u32);
    let patched = match val.get_mut("meta") {
        Some(serde_json::Value::Object(meta)) => {
            meta.insert("duration".into(), serde_json::json!(ms.to_string()));
            true
        }
        _ => false,
    };
    if !patched {
        return payload_b64.to_string();
    }
    match serde_json::to_vec(&val) {
        Ok(out) => b64_encode(&out),
        Err(_) => payload_b64.to_string(),
    }
}

/// 挑战求解失败时的 JS `c()` 回退串（vqd.go / 桌面端同款格式）。
///
/// 该串**不是**一个合法 token；按 §5.4 链路，求解失败应转入 override → browser → 503，
/// 暴露此函数是为了在测试中钉住与参考实现逐字节一致的回退格式。
pub fn fallback_payload(decoded_challenge: &str, err: &PowError, env: &PowEnv) -> String {
    let raw = format!(
        "{decoded}::{msg}::{stack}::{origin}",
        decoded = decoded_challenge,
        msg = err,
        stack = env.stack,
        origin = env.origin
    );
    b64_encode(raw.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b64_roundtrip() {
        let original = "hello \u{1f600}".as_bytes();
        let enc = b64_encode(original);
        assert_eq!(b64_decode(&enc).unwrap(), original);
    }

    #[test]
    fn sha256_base64_known_vector() {
        // sha256("abc") = ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad
        assert_eq!(
            sha256_base64("abc"),
            "ungWv48Bz+pBQUDeXa4iI7ADYaOWF3qctBD/YfIAFa0="
        );
    }

    #[test]
    fn fallback_format_matches_reference() {
        let env = PowEnv {
            user_agent: "ua".into(),
            origin: "https://duck.ai".into(),
            stack: "Error\nat l".into(),
        };
        let out = fallback_payload("CHALLENGE", &PowError::Timeout, &env);
        let decoded = String::from_utf8(b64_decode(&out).unwrap()).unwrap();
        assert_eq!(
            decoded,
            "CHALLENGE::challenge execution timed out::Error\nat l::https://duck.ai"
        );
    }

    #[test]
    fn align_duration_patches_meta_only() {
        let payload = r#"{"server_hashes":["a"],"client_hashes":["b","c","d"],"signals":{},"meta":{"v":"4","challenge_id":"x","timestamp":"1","debug":"CD","origin":"https://duck.ai","stack":"Error\nat l","duration":"0"}}"#;
        let b64 = b64_encode(payload.as_bytes());
        let out = align_duration(&b64);
        assert_ne!(out, b64, "应改写 duration");
        let v: serde_json::Value = serde_json::from_slice(&b64_decode(&out).unwrap()).unwrap();
        let ms: u32 = v["meta"]["duration"].as_str().unwrap().parse().unwrap();
        assert!((8..=20).contains(&ms), "duration {ms} 不在 8-20ms 观测区间");
        // 其余字段不动
        assert_eq!(v["meta"]["debug"], "CD");
        assert_eq!(v["client_hashes"], serde_json::json!(["b", "c", "d"]));
        // 非法输入原样返回
        assert_eq!(align_duration("not-base64-json"), "not-base64-json");
    }

    /// 线上挑战黄金重放（fixtures/challenge_fresh.b64，真实捕获）：
    /// 求解产物的键序、client_hashes 与 meta 必须与线上已验证的 Node 求解逐字节同形。
    #[test]
    fn golden_challenge_replay_fixture() {
        use serde_json::Value;

        let challenge_b64 = include_str!("../../../../fixtures/challenge_fresh.b64").trim();
        let ua = crate::headers::USER_AGENT;
        let origin = "https://duck.ai";
        let env = PowEnv {
            user_agent: ua.to_string(),
            origin: origin.to_string(),
            stack: crate::home::stack_for_bundle(crate::home::DEFAULT_ENTRY_BUNDLE_HASH),
        };

        let sol = RquickjsPow
            .solve(challenge_b64, &env)
            .expect("fixture challenge must solve");

        // payload = b64(JSON)，键序 server_hashes, signals, client_hashes, meta（mutation.js 语义）
        let decoded = String::from_utf8(b64_decode(&sol.payload_b64).expect("payload b64"))
            .expect("payload utf8");
        let payload: Value = serde_json::from_str(&decoded).expect("payload json");
        // 键序只能在原始 JSON 文本上断言：serde_json::Value 默认 BTreeMap 会把顺序抹平
        assert_key_order(&decoded, "payload");

        // client_hashes = 对挑战原始值逐个 SHA-256→base64；首项 = SHA256(UA)
        let hashes = payload["client_hashes"].as_array().expect("array");
        assert_eq!(
            hashes.len(),
            3,
            "client_hashes 恒 3 项 [UA, probe1, probe2]"
        );
        assert_eq!(
            hashes[0],
            Value::String(sha256_base64(ua)),
            "payload client_hashes[0] = SHA256(UA)"
        );
        for (i, h) in hashes.iter().enumerate() {
            assert_eq!(
                h,
                &Value::String(sha256_base64(&sol.raw_client_hashes[i])),
                "hash[{i}] 必须是原始值的 SHA-256→base64"
            );
        }

        // meta：origin/stack 覆盖为本环境，stack 指向真实 entry.duckai.<hash>.js
        let meta = &payload["meta"];
        assert_eq!(meta["origin"], Value::String(origin.to_string()));
        let stack = meta["stack"].as_str().expect("stack string");
        assert!(stack.contains("entry.duckai"), "stack: {stack}");
        assert!(
            stack.contains(crate::home::DEFAULT_ENTRY_BUNDLE_HASH),
            "stack 指向活 bundle: {stack}"
        );
        assert!(stack.starts_with("Error\nat "), "栈格式: {stack}");
        assert!(meta["duration"].is_string(), "duration 注入为字符串");
        // 挑战内 '1a60fa2d47f2cfa1' 是字符查找表源（_0x24ad2c），并非 challenge_id；
        // 实际 challenge_id = 字符串表索引 0x94 的解码结果，fixture 确定性固定
        assert_eq!(
            sol.challenge_id.as_deref(),
            Some("278b5846efcd9074734d2b1103c6d96be84491c3534f037269daf518505f164avz95n"),
            "challenge_id 必须与 meta 解码值一致"
        );
        assert_eq!(
            payload["meta"]["challenge_id"].as_str(),
            sol.challenge_id.as_deref(),
            "求解诊断 challenge_id 必须取自 meta.challenge_id"
        );

        // 确定性不变量：同一挑战 + 同一环境 → 逐字节可重放（jitter 仅影响 probes 的数值内容，
        // 但键序/长度/首项/元数据必须稳定；重放两次比对结构）
        let sol2 = RquickjsPow
            .solve(challenge_b64, &env)
            .expect("second solve");
        let decoded2 = String::from_utf8(b64_decode(&sol2.payload_b64).unwrap()).unwrap();
        assert_key_order(&decoded2, "payload2");
        let payload2: Value = serde_json::from_str(&decoded2).unwrap();
        assert_eq!(payload2["client_hashes"][0], hashes[0]);
        assert_eq!(payload2["meta"]["stack"], meta["stack"]);
    }

    /// 诊断（`#[ignore]`）：解一次真实线上 challenge，输出方案供离线比对。
    /// 用法：`LIVE_CHALLENGE=$(cat live.b64) LIVE_UA='...' cargo test -p duckai-protocol diag_solve_live -- --ignored --nocapture`
    #[test]
    #[ignore = "live diagnostic; needs LIVE_CHALLENGE + LIVE_UA"]
    fn diag_solve_live() {
        let challenge = std::env::var("LIVE_CHALLENGE").expect("LIVE_CHALLENGE");
        let ua = std::env::var("LIVE_UA").expect("LIVE_UA");
        let env = PowEnv {
            user_agent: ua,
            origin: "https://duck.ai".into(),
            stack: crate::home::stack_for_bundle(crate::home::DEFAULT_ENTRY_BUNDLE_HASH),
        };
        let sol = RquickjsPow
            .solve(challenge.trim(), &env)
            .expect("solve live challenge");
        println!("SOL_B64={}", sol.payload_b64);
        println!("CHALLENGE_ID={:?}", sol.challenge_id);
    }

    /// 原始 JSON 文本键序断言：server_hashes → signals → client_hashes → meta。
    fn assert_key_order(json: &str, what: &str) {
        let pos = |k: &str| {
            json.find(&format!("\"{k}\":"))
                .unwrap_or_else(|| panic!("{what} 缺少键 {k}: {json}"))
        };
        let (a, b, c, d) = (
            pos("server_hashes"),
            pos("signals"),
            pos("client_hashes"),
            pos("meta"),
        );
        assert!(
            a < b && b < c && c < d,
            "{what} 键序必须为 server_hashes, signals, client_hashes, meta：{json}"
        );
    }
}
