//! `browser` feature：可选的高保真适配器（chromiumoxide 驱动系统 Chrome）。
//!
//! 用途：纯 HTTP 路径被风控长期压制时的保底接入（§5.4 求解链末端前的一级）。
//! 策略：
//! - 懒启动（首次 `chat` 才拉起浏览器），带健康检查与崩溃重启；
//! - 打开 `DUCKAI_BASE` 首页 → `page.evaluate` 用页面自身 JS 引擎完成
//!   `fetch('/duckchat/v1/chat')` + ReadableStream 解帧 → **缓冲后按节奏重放**。
//!
//! 重放节奏：浏览器侧把 SSE 帧排干后缓存，本适配器按历史帧间隔（钳制在
//! 5ms~2s）逐帧 yield——对调用方仍是逐帧流（非一次性假流），但不保证逐毫秒
//! 保真；无浏览器进程内不可用的部分（页内 RSA/挑战）全部交给页面 JS 自己完成。
//!
//! 代理：浏览器模式仅支持直连出口（代理池调度结果为代理时自动降级回直连并在
//! 日志中提示——HTTP 主路径才是代理完整路径）。

use std::sync::Arc;
use std::time::Duration;

use chromiumoxide::Browser;
use futures::StreamExt;
use futures::stream;
use tokio::sync::Mutex;

use duckai_types::{ModelInfo, UpstreamError, UpstreamEvent};

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

/// 浏览器接入实现（`mode() == "browser"`）。
pub struct BrowserUpstream {
    cfg: BrowserConfig,
    pool: Arc<EgressPool>,
    browser: Mutex<Option<BrowserHandle>>,
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

        // 让页面自身完成会话与签名：JS 内 fetch /duckchat/v1/chat 并把 SSE 排成帧数组。
        // 帧间时间戳一并回传，供重放时恢复节奏。
        let payload = chat_via_page(&page, &req, self.cfg.new_chat).await?;
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

/// 在页面上下文执行会话脚本：构造请求体 → fetch → 解 SSE → 返回帧数组。
async fn chat_via_page(
    page: &chromiumoxide::Page,
    req: &UpstreamRequest,
    new_chat: bool,
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

    let script = r#"
    async ([url, body, sessionKey]) => {
      const out = { frames: [], gaps_ms: [], error: null };
      try {
        const t0 = performance.now();
        const resp = await fetch(url, {
          method: 'POST',
          headers: { 'content-type': 'application/json' },
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

    let arg = serde_json::json!([
        format!("{}/duckchat/v1/chat", url_of(page).await?),
        serde_json::to_value(&body).map_err(|err| UpstreamError::InvalidInput(err.to_string()))?,
        session_hint,
    ]);
    let raw: serde_json::Value = page
        .evaluate(format!(
            "({script})({})",
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
