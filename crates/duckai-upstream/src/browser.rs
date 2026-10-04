//! `browser` feature：可选的高保真适配器（chromiumoxide 驱动系统 Chrome）。
//!
//! 用途：纯 HTTP 路径被风控长期压制时的保底接入（§5.4 求解链末端前的一级）。
//! 策略：
//! - 懒启动（首次 `chat` 才拉起浏览器），带健康检查与崩溃重启；
//! - 打开 `DUCKAI_BASE` 首页 → `page.evaluate` 用页面自身 JS 引擎完成
//!   `fetch('/duckchat/v1/chat')` + ReadableStream 解帧 → **缓冲后按节奏重放**；
//! - 动态头与 HTTP 主路径同一协议栈（§5.4 求解链）：页内取 `/duckchat/v1/status`
//!   挑战 → 本地 rquickjs PoW → 注入 `x-vqd-hash-1` / `x-fe-version` /
//!   `x-fe-signals` / `x-ddg-journey-id`（对照真实抓包 duck.ai.har，缺失动态头
//!   会被上游按 ERR_BN_LIMIT 拒绝）。
//!
//! 重放节奏：浏览器侧把 SSE 帧排干后缓存，本适配器按历史帧间隔（钳制在
//! 5ms~2s）逐帧 yield——对调用方仍是逐帧流（非一次性假流），但不保证逐毫秒
//! 保真；无浏览器进程内不可用的部分（页内 RSA/挑战）全部交给页面 JS 自己完成。
//!
//! 代理：浏览器模式仅支持直连出口（代理池调度结果为代理时自动降级回直连并在
//! 日志中提示——HTTP 主路径才是代理完整路径）。

use std::sync::Arc;
use std::time::{Duration, Instant};

use chromiumoxide::Browser;
use futures::StreamExt;
use futures::stream;
use rand::Rng;
use tokio::sync::Mutex;

use duckai_protocol::home::{fallback_fe_meta, parse_home, stack_for_bundle};
use duckai_protocol::pow::{PowEngine, PowEnv, RquickjsPow};
use duckai_protocol::{FeMeta, VqdStore, fe_signals, journey_id};
use duckai_types::{EgressScope, ModelInfo, UpstreamError, UpstreamEvent};

use crate::cooldown::now_ms;
use crate::pool::EgressPool;
use crate::{FactoryConfig, UpstreamClient, UpstreamRequest, UpstreamStream};

/// 浏览器适配器配置。
#[derive(Debug, Clone)]
pub struct BrowserConfig {
    pub base: String,
    pub new_chat: bool,
    pub chrome_path: Option<String>,
}

impl BrowserConfig {
    pub fn from_factory(cfg: &FactoryConfig) -> Self {
        Self {
            base: cfg.base.trim_end_matches('/').to_string(),
            new_chat: cfg.new_chat,
            chrome_path: cfg.chrome_path.clone(),
        }
    }
}

type BrowserHandle = Browser;

/// 页面元数据（`x-fe-version` / bundle 栈）缓存 TTL：与 HTTP 主路径一致（30 分钟）。
const FE_TTL: Duration = Duration::from_secs(30 * 60);
/// 首页解析失败时的兜底缓存 TTL：与 HTTP 主路径一致（60 秒）。
const FE_FALLBACK_TTL: Duration = Duration::from_secs(60);

/// 页内取挑战脚本：`GET /duckchat/v1/status`（与页面同源、同 UA、同 TLS 指纹，
/// 响应头 `x-vqd-hash-1` 即服务端挑战；不可读头时回传 status/error）。
const STATUS_SCRIPT: &str = r#"
async (url) => {
  try {
    const r = await fetch(url, { method: 'GET', headers: { 'x-vqd-accept': '1' } });
    return {
      status: r.status,
      vqd: r.headers.get('x-vqd-hash-1'),
      retry_after: r.headers.get('retry-after'),
    };
  } catch (err) {
    return { status: 0, error: String(err) };
  }
}
"#;

/// 页内 chat 脚本：请求头与真实页面对照（duck.ai.har）——`accept: text/event-stream`、
/// `priority: u=1, i`，动态头（`x-vqd-hash-1` 等四个）由 Rust 侧求解后注入 `dynHeaders`。
const CHAT_SCRIPT: &str = r#"
    async ([url, body, sessionKey, dynHeaders]) => {
      const out = { frames: [], gaps_ms: [], error: null };
      try {
        const t0 = performance.now();
        const resp = await fetch(url, {
          method: 'POST',
          headers: Object.assign(
            {
              'content-type': 'application/json',
              'accept': 'text/event-stream',
              'priority': 'u=1, i',
            },
            dynHeaders || {},
          ),
          body: JSON.stringify(body),
          credentials: 'include',
        });
        if (!resp.ok && resp.status !== 200) {
          out.error = { kind: 'http', status: resp.status };
          return out;
        }
        const reader = resp.body.getReader();
        const dec = new TextDecoder();
        let buf = '';
        let last = performance.now();
        for (;;) {
          const { done, value } = await reader.read();
          if (done) break;
          buf += dec.decode(value, { stream: true });
          let idx;
          while ((idx = buf.search(/\n\n/)) >= 0) {
            const block = buf.slice(0, idx);
            buf = buf.slice(idx + 2);
            const dataLines = block.split(/\r?\n/)
              .filter(l => l.startsWith('data:'))
              .map(l => l.slice(5).replace(/^ /, ''));
            if (!dataLines.length) continue;
            const now = performance.now();
            out.gaps_ms.push(Math.round(now - last));
            last = now;
            out.frames.push(dataLines.join('\n'));
          }
        }
        out.session_key = sessionKey;
        out.elapsed = performance.now() - t0;
      } catch (err) {
        out.error = { kind: 'exception', message: String(err) };
      }
      return out;
    }
    "#;

/// 页外动态头会话：本地 PoW 求解 + vqd 缓存 + 页面元数据缓存。
///
/// 浏览器模式的一切上游网络 I/O 都走页面本身（保真），本结构只做**本地计算
/// 与缓存**，与 HTTP 主路径共用同一协议栈实现（`duckai_protocol` 的 pow/home/vqd）。
struct PageSession {
    pow: RquickjsPow,
    vqd: VqdStore,
    fe: Option<FeCache>,
}

/// 页面元数据缓存条目。
struct FeCache {
    meta: FeMeta,
    at: Instant,
    ttl: Duration,
}

impl PageSession {
    fn new(vqd_override: Option<String>) -> Self {
        Self {
            pow: RquickjsPow,
            vqd: VqdStore::new(vqd_override),
            fe: None,
        }
    }

    /// 组装本次 chat 的动态请求头（与真实页面抓包对照的四个头）。
    async fn dyn_headers(
        &mut self,
        page: &chromiumoxide::Page,
    ) -> Result<serde_json::Value, UpstreamError> {
        // location.origin：PoW meta.origin 与取挑战 URL 都以页面实际源为准
        let origin = url_of(page).await?;
        let meta = self.fe_meta(page).await;
        let vqd = match self.vqd.token(now_ms()) {
            Some(token) => token,
            None => {
                let challenge = self.challenge_via_page(page, &origin).await?;
                // PoW 环境与真实页同构：UA 必须是被驱动浏览器的真实 UA
                //（solution 的 client_hashes[0] = SHA256(UA)，与 chat 请求头强绑定）
                let env = PowEnv {
                    user_agent: page_user_agent(page).await?,
                    origin: origin.clone(),
                    stack: stack_for_bundle(&meta.bundle_hash),
                };
                match self.pow.solve(&challenge, &env) {
                    Ok(solution) => {
                        self.vqd.store(solution.payload_b64.clone(), now_ms());
                        solution.payload_b64
                    }
                    Err(err) => {
                        tracing::warn!(%err, "browser: vqd PoW 求解失败");
                        self.vqd.note_challenge_failure();
                        return Err(UpstreamError::ChallengeFailed);
                    }
                }
            }
        };
        let start = now_ms();
        let elapsed = 60 + rand::rng().random_range(0..120u64);
        Ok(dyn_headers_json(
            vqd,
            &meta.fe_version,
            &fe_signals(start, elapsed),
            &journey_id(),
        ))
    }

    /// 页内取挑战：`GET {origin}/duckchat/v1/status`（同源 fetch，保真）。
    async fn challenge_via_page(
        &self,
        page: &chromiumoxide::Page,
        origin: &str,
    ) -> Result<String, UpstreamError> {
        let url = format!("{origin}/duckchat/v1/status");
        let arg = serde_json::to_string(&url).unwrap_or_else(|_| "\"/duckchat/v1/status\"".into());
        let raw: serde_json::Value = page
            .evaluate(format!("({STATUS_SCRIPT})({arg})"))
            .await
            .map_err(|err| UpstreamError::Transport(format!("页内取挑战失败: {err}")))?
            .into_value::<serde_json::Value>()
            .map_err(|err| UpstreamError::Transport(err.to_string()))?;
        challenge_from_status(&raw)
    }

    /// 页面元数据：直接读已加载页面的 DOM（与页面构建强一致，零额外网络），
    /// 缓存 30 分钟；解析失败回落兜底常量（60 秒）。
    async fn fe_meta(&mut self, page: &chromiumoxide::Page) -> FeMeta {
        if let Some(cache) = self.fe.as_ref() {
            if cache.at.elapsed() < cache.ttl {
                return cache.meta.clone();
            }
        }
        let html: Option<String> = page
            .evaluate("document.documentElement.outerHTML")
            .await
            .ok()
            .and_then(|handle| handle.into_value::<String>().ok());
        let (meta, ttl) = fe_from_html(html.as_deref());
        self.fe = Some(FeCache {
            meta: meta.clone(),
            at: Instant::now(),
            ttl,
        });
        meta
    }
}

/// 动态头 JSON 组装（纯函数，键序即真实页面抓包对照表）。
fn dyn_headers_json(
    vqd: String,
    fe_version: &str,
    signals: &str,
    journey: &str,
) -> serde_json::Value {
    serde_json::json!({
        "x-vqd-hash-1": vqd,
        "x-fe-version": fe_version,
        "x-fe-signals": signals,
        "x-ddg-journey-id": journey,
    })
}

/// 页内取挑战结果 → 挑战串或错误（映射与 HTTP 主路径 §5.4 一致）。
fn challenge_from_status(res: &serde_json::Value) -> Result<String, UpstreamError> {
    let status = res.get("status").and_then(|v| v.as_u64()).unwrap_or(0) as u16;
    match status {
        200..=299 => res
            .get("vqd")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or(UpstreamError::ChallengeFailed),
        418 => Err(UpstreamError::Banned {
            scope: EgressScope::Direct,
        }),
        429 => {
            let retry_after = res
                .get("retry_after")
                .and_then(|v| v.as_str())
                .and_then(|s| s.trim().parse::<u64>().ok())
                .unwrap_or(300);
            Err(UpstreamError::RateLimited { retry_after })
        }
        0 => Err(UpstreamError::Transport(
            res.get("error")
                .and_then(|m| m.as_str())
                .unwrap_or("页内取挑战失败")
                .to_string(),
        )),
        _ => Err(UpstreamError::ChallengeFailed),
    }
}

/// 页面 HTML → 元数据 + 缓存时长（解析成功 30 分钟，失败兜底 60 秒）。
fn fe_from_html(html: Option<&str>) -> (FeMeta, Duration) {
    match html.and_then(parse_home) {
        Some(meta) => (meta, FE_TTL),
        None => (fallback_fe_meta(), FE_FALLBACK_TTL),
    }
}

/// 页面真实 User-Agent（PoW `client_hashes[0] = SHA256(UA)` 与 chat 请求头绑定）。
async fn page_user_agent(page: &chromiumoxide::Page) -> Result<String, UpstreamError> {
    page.evaluate("navigator.userAgent")
        .await
        .map_err(|err| UpstreamError::Transport(err.to_string()))?
        .into_value::<String>()
        .map_err(|err| UpstreamError::Transport(err.to_string()))
}

/// 浏览器接入实现（`mode() == "browser"`）。
pub struct BrowserUpstream {
    cfg: BrowserConfig,
    pool: Arc<EgressPool>,
    browser: Mutex<Option<BrowserHandle>>,
    session: Mutex<PageSession>,
}

impl BrowserUpstream {
    pub fn new(cfg: FactoryConfig, pool: Arc<EgressPool>) -> Result<Self, String> {
        let browser_cfg = BrowserConfig::from_factory(&cfg);
        if browser_cfg.base.is_empty() {
            return Err("DUCKAI_BASE 为空".to_string());
        }
        Ok(Self {
            cfg: browser_cfg,
            pool,
            browser: Mutex::new(None),
            session: Mutex::new(PageSession::new(cfg.vqd_override.clone())),
        })
    }

    /// 取一个可用页面：懒启动 + 存活检查（`pages()` 探活）+ 崩溃重启。
    /// 调用期间持有浏览器锁，返回前释放，评估/抓取阶段不阻塞其他会话建页。
    async fn take_page(&self) -> Result<chromiumoxide::Page, UpstreamError> {
        let mut guard = self.browser.lock().await;
        let alive = match guard.as_ref() {
            Some(browser) => browser.pages().await.is_ok(),
            None => false,
        };
        if !alive {
            *guard = Some(launch_chrome(&self.cfg).await?);
        }
        let browser = guard.as_ref().expect("browser just launched");
        browser
            .new_page(format!("{}/", self.cfg.base))
            .await
            .map_err(|err| UpstreamError::Transport(format!("打开首页失败: {err}")))
    }
}

/// 拉起 Chrome，并把 handler 事件循环挂到独立任务（chromiumoxide 0.9 的要求）。
async fn launch_chrome(cfg: &BrowserConfig) -> Result<Browser, UpstreamError> {
    use chromiumoxide::BrowserConfig as ChromiumConfig;
    let mut builder = ChromiumConfig::builder()
        .with_head()
        .arg("--disable-blink-features=AutomationControlled")
        .arg("--no-first-run");
    if let Some(path) = cfg.chrome_path.as_deref().filter(|p| !p.is_empty()) {
        builder = builder.chrome_executable(path);
    }
    let config = builder
        .build()
        .map_err(|err| UpstreamError::Transport(format!("浏览器配置错误: {err}")))?;
    let (browser, mut handler) = Browser::launch(config)
        .await
        .map_err(|err| UpstreamError::Transport(format!("启动浏览器失败: {err}")))?;
    tokio::spawn(async move {
        while let Some(res) = handler.next().await {
            if res.is_err() {
                break;
            }
        }
    });
    Ok(browser)
}

#[async_trait::async_trait]
impl UpstreamClient for BrowserUpstream {
    fn mode(&self) -> &'static str {
        "browser"
    }

    fn egress_pool(&self) -> Option<Arc<EgressPool>> {
        Some(self.pool.clone())
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, UpstreamError> {
        // 模型列表不依赖页内会话：沿用协议层快照（P1-8 兜底）
        Ok(duckai_types::model::ModelCatalog::snapshot()
            .list()
            .to_vec())
    }

    async fn chat(&self, req: UpstreamRequest) -> Result<UpstreamStream, UpstreamError> {
        let page = self.take_page().await?;

        // 动态头（x-vqd-hash-1 本地 PoW 求解 + x-fe-version/signals/journey）：
        // 对照真实抓包，缺这组头会被上游按 ERR_BN_LIMIT 拒绝（418）。
        let dyn_headers = {
            let mut session = self.session.lock().await;
            session.dyn_headers(&page).await?
        };

        // 让页面自身完成会话与签名：JS 内 fetch /duckchat/v1/chat 并把 SSE 排成帧数组。
        // 帧间时间戳一并回传，供重放时恢复节奏。
        let payload = chat_via_page(&page, &req, self.cfg.new_chat, &dyn_headers).await?;
        if let Some(err) = payload.error {
            return Err(err);
        }

        let events: Vec<UpstreamEvent> = payload.frames;
        let gaps: Vec<u64> = payload.gaps_ms;
        // 按历史节奏重放（钳制 5ms~2s），drop 即终止
        let stream = stream::unfold(
            (events.into_iter(), gaps.into_iter()),
            |(mut ev, mut gaps)| async move {
                let gap = gaps.next().unwrap_or(0).clamp(5, 2000);
                match ev.next() {
                    Some(event) => {
                        if gap > 0 {
                            tokio::time::sleep(Duration::from_millis(gap)).await;
                        }
                        Some((Ok(event), (ev, gaps)))
                    }
                    None => None,
                }
            },
        );
        Ok(Box::pin(stream))
    }

    async fn probe(&self) -> Result<(), UpstreamError> {
        let page = self.take_page().await?;
        let ok = page
            .evaluate("document.title.length > 0")
            .await
            .map_err(|err| UpstreamError::Transport(err.to_string()))?
            .into_value::<bool>()
            .map_err(|err| UpstreamError::Transport(err.to_string()))?;
        if ok {
            Ok(())
        } else {
            Err(UpstreamError::Transport("页面加载异常".into()))
        }
    }
}

/// 页内抓取结果。
struct PageChat {
    frames: Vec<UpstreamEvent>,
    gaps_ms: Vec<u64>,
    error: Option<UpstreamError>,
}

/// 在页面上下文执行会话脚本：构造请求体 → fetch（含动态头）→ 解 SSE → 返回帧数组。
async fn chat_via_page(
    page: &chromiumoxide::Page,
    req: &UpstreamRequest,
    new_chat: bool,
    dyn_headers: &serde_json::Value,
) -> Result<PageChat, UpstreamError> {
    use duckai_protocol::chat::session_fingerprint;
    use duckai_protocol::{build_chat_body_once, generate_encryption_jwk};

    // 复用协议层纯函数生成请求体（与 HTTP 路径同一实现，仅执行器不同）
    let jwk = generate_encryption_jwk().map_err(|err| UpstreamError::Upstream {
        status: 500,
        body: format!("RSA 加密密钥生成失败: {err}"),
    })?;
    let body = build_chat_body_once(req, &jwk, new_chat);
    let session_hint = req
        .session_hint
        .clone()
        .unwrap_or_else(|| session_fingerprint(&req.model));

    let arg = serde_json::json!([
        format!("{}/duckchat/v1/chat", url_of(page).await?),
        serde_json::to_value(&body).map_err(|err| UpstreamError::InvalidInput(err.to_string()))?,
        session_hint,
        dyn_headers,
    ]);
    let raw: serde_json::Value = page
        .evaluate(format!(
            "({CHAT_SCRIPT})({})",
            serde_json::to_string(&arg).unwrap_or_else(|_| "[]".into())
        ))
        .await
        .map_err(|err| UpstreamError::Transport(format!("页内执行失败: {err}")))?
        .into_value::<serde_json::Value>()
        .map_err(|err| UpstreamError::Transport(err.to_string()))?;

    // 错误归类
    if let Some(error) = raw.get("error").filter(|v| !v.is_null()) {
        let status = error.get("status").and_then(|v| v.as_u64()).unwrap_or(0) as u16;
        let err = match status {
            418 => UpstreamError::Banned {
                scope: duckai_types::EgressScope::Direct,
            },
            429 => UpstreamError::RateLimited { retry_after: 5 },
            400 => UpstreamError::ChallengeFailed,
            s if s >= 400 => UpstreamError::Upstream {
                status: s,
                body: error
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("browser chat failed")
                    .to_string(),
            },
            _ => UpstreamError::Transport(
                error
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("browser chat exception")
                    .to_string(),
            ),
        };
        return Ok(PageChat {
            frames: Vec::new(),
            gaps_ms: Vec::new(),
            error: Some(err),
        });
    }

    let mut frames = Vec::new();
    let mut events = Vec::new();
    if let Some(list) = raw.get("frames").and_then(|f| f.as_array()) {
        for f in list {
            let text = f.as_str().unwrap_or_default();
            if let duckai_protocol::sse::UpstreamFrame::Event(ev) =
                duckai_protocol::sse::decode_frame(text)
            {
                events.push(ev);
            }
        }
        frames = events;
    }
    let gaps_ms = raw
        .get("gaps_ms")
        .and_then(|g| g.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_u64()).collect())
        .unwrap_or_default();

    Ok(PageChat {
        frames,
        gaps_ms,
        error: None,
    })
}

/// 取页面当前 URL 的 origin（首页打开后基址）。
async fn url_of(page: &chromiumoxide::Page) -> Result<String, UpstreamError> {
    let url: String = page
        .evaluate("location.origin")
        .await
        .map_err(|err| UpstreamError::Transport(err.to_string()))?
        .into_value::<String>()
        .map_err(|err| UpstreamError::Transport(err.to_string()))?;
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 回归钉：页内 chat 脚本必须带真实抓包（duck.ai.har）对照出的头。
    /// 曾因缺 `accept: text/event-stream` / 动态头被上游按 418 ERR_BN_LIMIT 拒绝。
    #[test]
    fn chat_script_matches_real_capture_headers() {
        for needle in [
            "'accept': 'text/event-stream'",
            "'priority': 'u=1, i'",
            "dynHeaders",
        ] {
            assert!(
                CHAT_SCRIPT.contains(needle),
                "CHAT_SCRIPT 缺真实页面对照头: {needle}"
            );
        }
        for header in [
            "x-vqd-hash-1",
            "x-fe-version",
            "x-fe-signals",
            "x-ddg-journey-id",
        ] {
            let json = dyn_headers_json("tok".into(), "v", "s", "j");
            assert!(
                json.get(header).is_some(),
                "动态头缺 {header}（对照 duck.ai.har）"
            );
        }
    }

    #[test]
    fn status_script_uses_vqd_accept() {
        assert!(STATUS_SCRIPT.contains("x-vqd-accept"));
        assert!(STATUS_SCRIPT.contains("x-vqd-hash-1"));
    }

    #[test]
    fn challenge_from_status_maps_upstream_shapes() {
        // 200 + 挑战
        let ok = serde_json::json!({"status": 200, "vqd": "  challenge-b64  "});
        assert_eq!(challenge_from_status(&ok).unwrap(), "challenge-b64");
        // 200 但缺头 → 挑战失败
        let missing = serde_json::json!({"status": 200, "vqd": null});
        assert!(matches!(
            challenge_from_status(&missing),
            Err(UpstreamError::ChallengeFailed)
        ));
        // 418 → 出口受限（直连）
        let banned = serde_json::json!({"status": 418});
        assert!(matches!(
            challenge_from_status(&banned),
            Err(UpstreamError::Banned {
                scope: EgressScope::Direct
            })
        ));
        // 429 → 限流，带 retry-after
        let limited = serde_json::json!({"status": 429, "retry_after": "7"});
        assert!(matches!(
            challenge_from_status(&limited),
            Err(UpstreamError::RateLimited { retry_after: 7 })
        ));
        let limited_default = serde_json::json!({"status": 429, "retry_after": null});
        assert!(matches!(
            challenge_from_status(&limited_default),
            Err(UpstreamError::RateLimited { retry_after: 300 })
        ));
        // 页面 fetch 异常 → 传输错误带原因
        let transport = serde_json::json!({"status": 0, "error": "Failed to fetch"});
        assert!(matches!(
            challenge_from_status(&transport),
            Err(UpstreamError::Transport(msg)) if msg.contains("Failed to fetch")
        ));
        // 其他 4xx/5xx → 挑战失败
        let other = serde_json::json!({"status": 500});
        assert!(matches!(
            challenge_from_status(&other),
            Err(UpstreamError::ChallengeFailed)
        ));
    }

    #[test]
    fn fe_from_html_parses_or_falls_back() {
        let html = r#"<html lang="zh-CN" data-version-tag="serp_20261002_113931_ET" data-version-sha="16077f4eafa643a1d3b77a0392138879174cae48"><head><script src="/dist/duckai-dist/entry.duckai.7ea8f9b262254b03739.js"></script></head></html>"#;
        let (meta, ttl) = fe_from_html(Some(html));
        assert_eq!(
            meta.fe_version,
            "serp_20261002_113931_ET-16077f4eafa643a1d3b77a0392138879174cae48"
        );
        assert_eq!(meta.bundle_hash, "7ea8f9b262254b03739");
        assert_eq!(ttl, FE_TTL);

        let (fallback, fallback_ttl) = fe_from_html(None);
        assert!(!fallback.fe_version.is_empty(), "解析失败须回落兜底常量");
        assert_eq!(fallback_ttl, FE_FALLBACK_TTL);
    }
}
