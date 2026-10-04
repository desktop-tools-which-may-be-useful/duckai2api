//! 上游请求头构造（§2.2.3 header 顺序）。
//!
//! Duck.ai 网关对头序、指纹头与动态头都有校验：`x-fe-version` 来自首页版本属性，
//! `x-fe-signals` 是 base64 JSON 遥测，`x-ddg-journey-id` 每请求随机，`x-vqd-hash-1`
//! 来自挑战求解（见 [`crate::pow`]）。本模块只做确定性组装，取挑战与抓首页由上游层驱动。

use base64::Engine as _;
use rand::Rng as _;

/// 浏览器指纹 UA（线上实测被接受的 Chrome/152 桌面 UA）。
pub const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/152.0.0.0 Safari/537.36";
/// `sec-ch-ua` Client Hints（与 UA 同批线上实测值）。
pub const SEC_CH_UA: &str =
    "\"Not=A?Brand\";v=\"99\", \"Google Chrome\";v=\"151\", \"Chromium\";v=\"151\"";
/// `x-fe-version` 遥测字段名（signals JSON 键序与线上一致）。
const SIGNAL_EVENTS: [(&str, u32, u32); 2] = [
    ("recentChatsImpression", 40, 60),
    ("recentChatsPopoverImpression", 50, 70),
];

/// 生成 16 字节 hex（32 字符）的 journey id。
pub fn journey_id() -> String {
    let n: u128 = rand::rng().random();
    format!("{n:032x}")
}

/// `x-fe-signals`：base64(JSON) 的 FE 遥测（字段与线上抓取一致）。
pub fn fe_signals(start_ms: u64, elapsed_ms: u64) -> String {
    let events: Vec<serde_json::Value> = SIGNAL_EVENTS
        .iter()
        .map(|(name, lo, hi)| {
            let delta = rand::rng().random_range(*lo..=*hi);
            serde_json::json!({ "name": name, "delta": delta })
        })
        .collect();
    let payload = serde_json::json!({
        "start": start_ms,
        "events": events,
        "end": elapsed_ms,
    });
    base64::engine::general_purpose::STANDARD.encode(payload.to_string().as_bytes())
}

/// 浏览器形态 chat 的 `x-fe-signals`：onboarding→startNewChat_free 事件表。
///
/// 与两次**已证实成功**的 chat 请求（duck.ai.har entry[11] 20:36Z、自然流程 22:21Z）
/// 的表结构一致：`onboarding_impression` → `action(trusted)` → `onboarding_impression`
/// → `onboarding_finish` → `startNewChat_free`，事件 delta 单调递增、`end` 收尾于
/// 最后事件之后。HTTP 主路径继续用 [`fe_signals`]（已线上验证 200）；本表只服务
/// 浏览器模式——服务端对 Chrome 形态请求校验遥测表名，`recentChats*` 两张表不同源。
pub fn fe_signals_browser(start_ms: u64) -> String {
    const EVENTS: [(&str, u32, u32); 5] = [
        ("onboarding_impression", 80, 130),
        ("action", 900, 1_100),
        ("onboarding_impression", 9_500, 10_500),
        ("onboarding_finish", 11_900, 12_300),
        ("startNewChat_free", 12_350, 12_700),
    ];
    let mut last = 0u32;
    let events: Vec<serde_json::Value> = EVENTS
        .iter()
        .map(|(name, lo, hi)| {
            let delta = rand::rng().random_range(*lo..=*hi).max(last + 1);
            last = delta;
            if *name == "action" {
                serde_json::json!({ "name": name, "delta": delta, "trusted": true })
            } else {
                serde_json::json!({ "name": name, "delta": delta })
            }
        })
        .collect();
    let end = last + rand::rng().random_range(1..=3u32);
    let payload = serde_json::json!({
        "start": start_ms,
        "events": events,
        "end": end,
    });
    base64::engine::general_purpose::STANDARD.encode(payload.to_string().as_bytes())
}

/// 状态探测（`GET /`）请求头——origin/referer/accept-language 必带（线上验证必需）。
pub fn build_status_headers(origin: &str) -> Vec<(String, String)> {
    vec![
        (
            "accept".into(),
            "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8"
                .into(),
        ),
        ("accept-language".into(), "en-US,en;q=0.9".into()),
        ("origin".into(), origin.to_string()),
        ("referer".into(), format!("{origin}/")),
        ("sec-ch-ua".into(), SEC_CH_UA.into()),
        ("sec-ch-ua-mobile".into(), "?0".into()),
        ("sec-ch-ua-platform".into(), "\"Linux\"".into()),
        ("sec-fetch-dest".into(), "document".into()),
        ("sec-fetch-mode".into(), "navigate".into()),
        ("sec-fetch-site".into(), "same-origin".into()),
        ("user-agent".into(), USER_AGENT.into()),
    ]
}

/// 一次发问所需的全部请求头（顺序即 §2.2.3 表；大小写按此写入 wire）。
///
/// * `vqd` —— `X-Vqd-Hash-1`（求解产物）；
/// * `fe_version` —— 首页 `<tag>-<sha>`；
/// * `signals` —— [`fe_signals`] 的输出；
/// * `journey` —— [`journey_id`] 的输出。
pub fn build_chat_headers(
    origin: &str,
    vqd: &str,
    fe_version: &str,
    signals: &str,
    journey: &str,
) -> Vec<(String, String)> {
    let mut headers = vec![
        ("content-type".to_string(), "application/json".to_string()),
        (
            "accept".to_string(),
            "text/event-stream,application/json".to_string(),
        ),
        ("accept-language".to_string(), "en-US,en;q=0.9".to_string()),
        ("x-fe-version".to_string(), fe_version.to_string()),
        ("x-fe-signals".to_string(), signals.to_string()),
        ("x-vqd-hash-1".to_string(), vqd.to_string()),
        ("x-ddg-journey-id".to_string(), journey.to_string()),
        ("origin".to_string(), origin.to_string()),
        ("referer".to_string(), format!("{origin}/")),
        ("sec-ch-ua".to_string(), SEC_CH_UA.to_string()),
        ("sec-ch-ua-mobile".to_string(), "?0".to_string()),
        ("sec-ch-ua-platform".to_string(), "\"Linux\"".to_string()),
        ("sec-fetch-dest".to_string(), "empty".to_string()),
        ("sec-fetch-mode".to_string(), "cors".to_string()),
        ("sec-fetch-site".to_string(), "same-origin".to_string()),
        ("user-agent".to_string(), USER_AGENT.to_string()),
        ("priority".to_string(), "u=1, i".to_string()),
    ];
    headers.shrink_to_fit();
    headers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journey_is_16_bytes_hex() {
        let id = journey_id();
        assert_eq!(id.len(), 32);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
        // 随机性：两次几乎必然不同
        assert_ne!(journey_id(), journey_id());
    }

    #[test]
    fn signals_decode_to_expected_json() {
        let b64 = fe_signals(1_791_118_047_378, 396_244);
        let raw = base64::engine::general_purpose::STANDARD
            .decode(&b64)
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(v["start"], 1_791_118_047_378u64);
        assert_eq!(v["end"], 396_244u64);
        let events = v["events"].as_array().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["name"], "recentChatsImpression");
        let delta = events[0]["delta"].as_u64().unwrap();
        assert!((40..=60).contains(&delta), "delta {delta} out of range");
    }

    #[test]
    fn signals_browser_matches_proven_onboarding_table() {
        let b64 = fe_signals_browser(1_791_146_183_838);
        let raw = base64::engine::general_purpose::STANDARD
            .decode(&b64)
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(v["start"], 1_791_146_183_838u64);
        let events = v["events"].as_array().unwrap();
        assert_eq!(events.len(), 5);
        let names: Vec<&str> = events.iter().map(|e| e["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            [
                "onboarding_impression",
                "action",
                "onboarding_impression",
                "onboarding_finish",
                "startNewChat_free"
            ]
        );
        assert_eq!(events[1]["trusted"], true);
        // delta 单调递增，end 收尾于最后事件之后（与 HAR entry[11] 同构）
        let mut prev = 0u64;
        for e in events {
            let d = e["delta"].as_u64().unwrap();
            assert!(d > prev, "deltas must increase: {d} <= {prev}");
            prev = d;
        }
        let end = v["end"].as_u64().unwrap();
        assert!(end > prev, "end {end} must exceed last delta {prev}");
    }

    #[test]
    fn chat_header_order_matches_spec() {
        let h = build_chat_headers("https://duck.ai", "V", "F", "S", "J");
        let names: Vec<&str> = h.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "content-type",
                "accept",
                "accept-language",
                "x-fe-version",
                "x-fe-signals",
                "x-vqd-hash-1",
                "x-ddg-journey-id",
                "origin",
                "referer",
                "sec-ch-ua",
                "sec-ch-ua-mobile",
                "sec-ch-ua-platform",
                "sec-fetch-dest",
                "sec-fetch-mode",
                "sec-fetch-site",
                "user-agent",
                "priority",
            ]
        );
        let map: std::collections::HashMap<_, _> = h.into_iter().collect();
        assert_eq!(map["x-vqd-hash-1"], "V");
        assert_eq!(map["x-fe-version"], "F");
        assert_eq!(map["x-ddg-journey-id"], "J");
    }
}
