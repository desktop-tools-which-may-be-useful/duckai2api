//! 纯 HTTP 适配器——**默认主路径**（ARCHITECTURE §2.2 全流程，无 Playwright 依赖）。
//!
//! ```text
//! acquire egress → ensure_vqd（本地 rquickjs 求解链：缓存 → override → 取挑战）
//!   → build_chat_body + 17 头（x-fe-version 30min / x-fe-signals / journey）
//!   → POST /duckchat/v1/chat → 200: SSE → 事件流（真实节奏，无假流）
//!   → 418: Banned 换端重放（≤3 次） / 429: 记账 + 尊重 Retry-After
//!   → 400/ERR_CHALLENGE: VQD 失效重取（≤3，不消耗 egress）
//! ```
//!
//! 挑战类错误**不换 egress 不记账**；418/429 才推进 §6 状态机。流中错误原样 yield，
//! 调用方（API 层）映射 HTTP 语义（§9 错误映射表）。

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use futures::stream::{BoxStream, StreamExt};
use rand::Rng;
use serde::Deserialize;
use tokio::sync::RwLock;

use duckai_protocol::headers::USER_AGENT;
use duckai_protocol::home::{fallback_fe_meta, parse_home, stack_for_bundle};
use duckai_protocol::pow::{PowEngine, PowEnv, RquickjsPow};
use duckai_protocol::sse::{SseParser, UpstreamFrame, decode_frame};
use duckai_protocol::{
    FeMeta, RsaJwk, StickyConversation, VqdStore, build_chat_body, build_chat_headers,
    build_status_headers, fe_signals, generate_encryption_jwk, journey_id,
};
use duckai_types::model::ModelCatalog;
use duckai_types::{EgressScope, ModelInfo, ModelSource, UpstreamError};

use crate::cooldown::{RATE_LIMIT_BASE_SECS, now_ms};
use crate::pool::{EgressHandle, EgressPool};
use crate::{FactoryConfig, UpstreamRequest, UpstreamStream};

/// 418 换端重放预算（§2.2：重试 ≤3 → 换端 ≤2）。
const MAX_EGRESS_SWITCHES: u32 = 2;
/// 挑战失效重取预算（§2.2：≤3）。
const MAX_CHALLENGE_TRIES: u32 = 3;
/// `Retry-After` 超过该秒数不再原地等待，直接向上抛 429。
const SHORT_RETRY_SECS: u64 = 5;
/// x-fe-version / 首页缓存 TTL（§2.2.3：30 分钟）。
const FE_TTL: Duration = Duration::from_secs(30 * 60);
/// 模型目录缓存 TTL（§7：上游拉取结果缓存）。
const MODELS_TTL: Duration = Duration::from_secs(30 * 60);
/// 整体请求超时（§9：Timeout → 502）。
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

/// 适配器配置（从 [`FactoryConfig`] 派生；测试可直接构造）。
#[derive(Debug, Clone)]
pub struct HttpConfig {
    pub base: String,
    pub new_chat: bool,
    pub timeout: Duration,
}

impl HttpConfig {
    pub fn from_factory(cfg: &FactoryConfig) -> Self {
        Self {
            base: cfg.base.trim_end_matches('/').to_string(),
            new_chat: cfg.new_chat,
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

#[derive(Debug, Clone)]
struct FeCache {
    meta: FeMeta,
    at: Instant,
    /// 抓取失败的兜底缓存短 TTL（60s），避免每次发问都轰首页。
    ttl: Duration,
}

struct ModelsCache {
    models: Vec<ModelInfo>,
    at: Instant,
}

/// `attempt()` 的分类结果。
enum Attempt {
    /// 200 + SSE：流已建好，e­gress 归流终结时释放。
    Stream(BoxStream<'static, Result<duckai_types::UpstreamEvent, UpstreamError>>),
    Banned {
        reason: &'static str,
        scope: EgressScope,
    },
    RateLimited {
        retry_after: Option<u64>,
    },
    /// 挑战/求解失败：失效重取，不换 egress。
    Challenge,
    /// 不可重试的失败（传输/超时/上游 5xx/输入）。
    Failed(UpstreamError),
}

/// VQD 保证步骤的失败分类。
enum VqdStep {
    Banned(&'static str, EgressScope),
    RateLimited(Option<u64>),
    Challenge,
    /// 传输层失败（连接拒绝/超时）：映射 502/超时，不按挑战重取。
    Failed(UpstreamError),
}

/// 流终止动作（弹出事件时执行一次：记账 + 归还额度）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PostAction {
    None,
    Success,
    Banned(&'static str),
    RateLimited(Option<u64>),
}

type PendingItem = (
    Result<duckai_types::UpstreamEvent, UpstreamError>,
    PostAction,
);

struct StreamCtx {
    parser: SseParser,
    inner: BoxStream<'static, Result<bytes::Bytes, reqwest::Error>>,
    pending: VecDeque<PendingItem>,
    stopped: bool,
    released: bool,
    pool: Arc<EgressPool>,
    handle: EgressHandle,
}

impl StreamCtx {
    fn feed(&mut self, text: &str) {
        for payload in self.parser.push(text) {
            self.feed_payload(payload);
        }
    }

    fn feed_payload(&mut self, payload: String) {
        match decode_frame(&payload) {
            UpstreamFrame::Event(ev) => {
                let done = duckai_types::UpstreamEvent::is_done(&ev);
                if done {
                    self.stopped = true;
                    self.pending.push_back((Ok(ev), PostAction::Success));
                } else {
                    self.pending.push_back((Ok(ev), PostAction::None));
                }
            }
            UpstreamFrame::Error {
                status,
                kind,
                override_code,
            } => {
                let err =
                    classify_inline(status, &kind, override_code.as_deref(), self.handle.scope);
                let post = post_for(&err);
                self.stopped = true;
                self.pending.push_back((Err(err), post));
            }
            UpstreamFrame::Ignore => {}
        }
    }

    fn finish(&mut self, action: PostAction) {
        if self.released {
            return;
        }
        self.released = true;
        match action {
            PostAction::None => {}
            PostAction::Success => self.pool.record_success(&self.handle),
            PostAction::Banned(reason) => self.pool.record_banned(&self.handle, reason),
            PostAction::RateLimited(ra) => self.pool.record_rate_limited(&self.handle, ra),
        }
        self.pool.release(&self.handle);
    }
}

/// 纯 HTTP 接入实现（`mode() == "http"`）。
pub struct HttpUpstream {
    cfg: HttpConfig,
    pool: Arc<EgressPool>,
    vqd: VqdStore,
    pow: RquickjsPow,
    jwk: StdMutex<Option<RsaJwk>>,
    sticky: StickyConversation,
    fe: RwLock<Option<FeCache>>,
    models: RwLock<Option<ModelsCache>>,
    clients: StdMutex<HashMap<Option<String>, Arc<reqwest::Client>>>,
}

impl HttpUpstream {
    pub fn new(cfg: FactoryConfig, pool: Arc<EgressPool>) -> Result<Self, String> {
        let http_cfg = HttpConfig::from_factory(&cfg);
        if http_cfg.base.is_empty() {
            return Err("DUCKAI_BASE 为空".to_string());
        }
        Ok(Self {
            vqd: VqdStore::new(cfg.vqd_override),
            cfg: http_cfg,
            pool,
            pow: RquickjsPow,
            jwk: StdMutex::new(None),
            sticky: StickyConversation::new(),
            fe: RwLock::new(None),
            models: RwLock::new(None),
            clients: StdMutex::new(HashMap::new()),
        })
    }

    /// 观测位（`/health`）：(vqd_valid, fe_version_age_secs)。
    pub fn observability(&self) -> (bool, Option<u64>) {
        let valid = self.vqd.has_valid_token(now_ms());
        let age = self
            .fe
            .try_read()
            .ok()
            .and_then(|g| g.as_ref().map(|c| c.at.elapsed().as_secs()));
        (valid, age)
    }

    pub fn pool(&self) -> &Arc<EgressPool> {
        &self.pool
    }

    // ---------- 传输层 ----------

    fn client_for(&self, handle: &EgressHandle) -> Arc<reqwest::Client> {
        let key = handle.proxy_url.clone();
        if let Ok(mut map) = self.clients.lock() {
            if let Some(c) = map.get(&key) {
                return c.clone();
            }
            let client = self.build_client(&key);
            map.insert(key, client.clone());
            return client;
        }
        self.build_client(&handle.proxy_url)
    }

    fn build_client(&self, proxy_url: &Option<String>) -> Arc<reqwest::Client> {
        let mut builder = reqwest::Client::builder()
            .timeout(self.cfg.timeout)
            .connect_timeout(Duration::from_secs(10));
        if let Some(url) = proxy_url {
            match reqwest::Proxy::all(url) {
                Ok(proxy) => builder = builder.proxy(proxy),
                // 池层已校验 scheme；万一客户端构造失败则回落直连并留日志
                Err(err) => tracing::warn!(%err, "proxy 构造失败，回落直连"),
            }
        }
        let built = builder.build().unwrap_or_default();
        Arc::new(built)
    }

    // ---------- x-fe-version / 首页 ----------

    async fn fe_meta(&self) -> FeMeta {
        {
            let guard = self.fe.read().await;
            if let Some(c) = guard.as_ref() {
                if c.at.elapsed() < c.ttl {
                    return c.meta.clone();
                }
            }
        }
        let fetched = self.fetch_home().await;
        let (meta, ttl) = match fetched {
            Some(meta) => (meta, FE_TTL),
            None => (fallback_fe_meta(), Duration::from_secs(60)),
        };
        {
            let mut guard = self.fe.write().await;
            *guard = Some(FeCache {
                meta: meta.clone(),
                at: Instant::now(),
                ttl,
            });
        }
        meta
    }

    async fn fetch_home(&self) -> Option<FeMeta> {
        let handle = self.pool.acquire(None)?;
        let client = self.client_for(&handle);
        let url = format!("{}/", self.cfg.base);
        let mut headers = header_map(&build_status_headers(&self.cfg.base));
        headers.insert(
            reqwest::header::ACCEPT,
            reqwest::header::HeaderValue::from_static("text/html"),
        );
        let out = match client.get(url).headers(headers).send().await {
            Ok(resp) if resp.status().is_success() => {
                resp.text().await.ok().and_then(|t| parse_home(&t))
            }
            _ => None,
        };
        self.pool.release(&handle);
        out
    }

    // ---------- VQD / 挑战求解链（§5.4） ----------

    async fn ensure_vqd(&self, handle: &EgressHandle) -> Result<String, VqdStep> {
        if let Some(token) = self.vqd.token(now_ms()) {
            return Ok(token);
        }
        // 挑战取回与求解只需要首页 bundle 指纹；fe_meta 自带短 TTL 缓存
        let meta = self.fe_meta().await;

        let client = self.client_for(handle);
        let mut headers = header_map(&build_status_headers(&self.cfg.base));
        headers.insert(
            "x-vqd-accept",
            reqwest::header::HeaderValue::from_static("1"),
        );
        let url = format!("{}/duckchat/v1/status", self.cfg.base);
        let resp = match client.get(url).headers(headers).send().await {
            Ok(resp) => resp,
            Err(err) if err.is_timeout() => return Err(VqdStep::Failed(UpstreamError::Timeout)),
            Err(err) => return Err(VqdStep::Failed(UpstreamError::Transport(err.to_string()))),
        };
        match resp.status().as_u16() {
            418 => return Err(VqdStep::Banned("418 ERR_BN_LIMIT", handle.scope)),
            429 => return Err(VqdStep::RateLimited(retry_after_secs(resp.headers()))),
            s if !(200..300).contains(&s) => return Err(VqdStep::Challenge),
            _ => {}
        }
        let challenge = resp
            .headers()
            .get("x-vqd-hash-1")
            .and_then(|v| v.to_str().ok())
            .and_then(VqdStore::extract_challenge)
            .ok_or(VqdStep::Challenge)?
            .to_string();

        let env = PowEnv {
            user_agent: USER_AGENT.to_string(),
            origin: self.cfg.base.clone(),
            stack: stack_for_bundle(&meta.bundle_hash),
        };
        match self.pow.solve(&challenge, &env) {
            Ok(solution) => {
                self.vqd.store(solution.payload_b64.clone(), now_ms());
                Ok(solution.payload_b64)
            }
            Err(_) => Err(VqdStep::Challenge),
        }
    }

    // ---------- 单次尝试 ----------

    async fn attempt(&self, handle: &EgressHandle, req: &UpstreamRequest) -> Attempt {
        let vqd = match self.ensure_vqd(handle).await {
            Ok(token) => token,
            Err(VqdStep::Banned(reason, scope)) => return Attempt::Banned { reason, scope },
            Err(VqdStep::RateLimited(retry_after)) => return Attempt::RateLimited { retry_after },
            Err(VqdStep::Challenge) => return Attempt::Challenge,
            Err(VqdStep::Failed(err)) => return Attempt::Failed(err),
        };

        let jwk = match self.jwk() {
            Ok(jwk) => jwk,
            Err(err) => return Attempt::Failed(err),
        };
        let body = build_chat_body(req, &self.sticky, &jwk, self.cfg.new_chat);

        let meta = self.fe_meta().await;
        let start = now_ms();
        let elapsed = 60 + rand::rng().random_range(0..120u64);
        let signals = fe_signals(start, elapsed);
        let journey = journey_id();
        let headers = header_map(&build_chat_headers(
            &self.cfg.base,
            &vqd,
            &meta.fe_version,
            &signals,
            &journey,
        ));

        let client = self.client_for(handle);
        let url = format!("{}/duckchat/v1/chat", self.cfg.base);
        let resp = match client.post(url).headers(headers).json(&body).send().await {
            Ok(resp) => resp,
            Err(err) if err.is_timeout() => return Attempt::Failed(UpstreamError::Timeout),
            Err(err) => {
                return Attempt::Failed(UpstreamError::Transport(err.to_string()));
            }
        };

        match resp.status().as_u16() {
            200 => Attempt::Stream(self.stream_from(resp, handle.clone())),
            418 => Attempt::Banned {
                reason: "418 ERR_BN_LIMIT",
                scope: handle.scope,
            },
            429 => Attempt::RateLimited {
                retry_after: retry_after_secs(resp.headers()),
            },
            status => {
                let text = resp.text().await.unwrap_or_default();
                let upper = text.to_ascii_uppercase();
                if status == 400 && (upper.contains("ERR_CHALLENGE") || upper.contains("CHALLENGE"))
                {
                    // 400 + 挑战错误 → 失效重取（§2.2 第 8 步）
                    return Attempt::Challenge;
                }
                Attempt::Failed(UpstreamError::Upstream {
                    status,
                    body: truncate(&text, 500),
                })
            }
        }
    }

    fn stream_from(&self, resp: reqwest::Response, handle: EgressHandle) -> UpstreamStream {
        let inner: BoxStream<'static, Result<bytes::Bytes, reqwest::Error>> =
            Box::pin(resp.bytes_stream());
        let ctx = StreamCtx {
            parser: SseParser::new(),
            inner,
            pending: VecDeque::new(),
            stopped: false,
            released: false,
            pool: self.pool.clone(),
            handle,
        };
        Box::pin(futures::stream::unfold(ctx, |mut st| async move {
            loop {
                if let Some((item, action)) = st.pending.pop_front() {
                    if action != PostAction::None || item.is_err() {
                        st.finish(action);
                    }
                    return Some((item, st));
                }
                if st.stopped {
                    st.finish(PostAction::None);
                    return None;
                }
                match st.inner.next().await {
                    Some(Ok(bytes)) => {
                        let text = String::from_utf8_lossy(&bytes);
                        st.feed(&text);
                    }
                    Some(Err(err)) => {
                        st.stopped = true;
                        st.pending.push_back((
                            Err(UpstreamError::Transport(err.to_string())),
                            PostAction::None,
                        ));
                    }
                    None => {
                        for payload in st.parser.flush() {
                            st.feed_payload(payload);
                        }
                        st.stopped = true;
                    }
                }
            }
        }))
    }

    fn jwk(&self) -> Result<RsaJwk, UpstreamError> {
        let generate = || {
            generate_encryption_jwk().map_err(|_| UpstreamError::Upstream {
                status: 500,
                body: "RSA 加密密钥生成失败".to_string(),
            })
        };
        if let Ok(mut guard) = self.jwk.lock() {
            if let Some(jwk) = guard.as_ref() {
                return Ok(jwk.clone());
            }
            let fresh = generate()?;
            *guard = Some(fresh.clone());
            return Ok(fresh);
        }
        generate()
    }

    /// 全部出口不可用时的错误分类（Banned 优先，其次 429 冷却余量）。
    fn unavailable_error(&self) -> UpstreamError {
        let (banned, cooldown) = self.pool.unavailable_hint();
        match banned {
            Some(scope) => UpstreamError::Banned { scope },
            None => UpstreamError::RateLimited {
                retry_after: cooldown.unwrap_or(RATE_LIMIT_BASE_SECS),
            },
        }
    }
}

#[async_trait::async_trait]
impl crate::UpstreamClient for HttpUpstream {
    fn mode(&self) -> &'static str {
        "http"
    }

    fn observability(&self) -> (bool, Option<u64>) {
        HttpUpstream::observability(self)
    }

    fn egress_pool(&self) -> Option<Arc<EgressPool>> {
        Some(self.pool.clone())
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, UpstreamError> {
        {
            let guard = self.models.read().await;
            if let Some(cache) = guard.as_ref().filter(|c| c.at.elapsed() < MODELS_TTL) {
                return Ok(cache.models.clone());
            }
        }
        let fetched = self.fetch_models().await;
        let models = match fetched {
            Some(models) => models,
            None => ModelCatalog::snapshot().list().to_vec(),
        };
        {
            let mut guard = self.models.write().await;
            *guard = Some(ModelsCache {
                models: models.clone(),
                at: Instant::now(),
            });
        }
        Ok(models)
    }

    async fn chat(&self, req: UpstreamRequest) -> Result<UpstreamStream, UpstreamError> {
        let mut banned_switches = 0u32;
        let mut rate_tries = 0u32;
        let mut challenge_tries = 0u32;

        loop {
            let handle = match self.pool.acquire(req.session_hint.as_deref()) {
                Some(handle) => handle,
                None => return Err(self.unavailable_error()),
            };
            match self.attempt(&handle, &req).await {
                // 成功：流接管 egress 额度，结束时自行释放 + 记成功
                Attempt::Stream(stream) => return Ok(stream),
                Attempt::Failed(err) => {
                    self.pool.release(&handle);
                    return Err(err);
                }
                Attempt::Banned { reason, scope } => {
                    self.pool.release(&handle);
                    self.pool.record_banned(&handle, reason);
                    banned_switches += 1;
                    if banned_switches > MAX_EGRESS_SWITCHES {
                        return Err(UpstreamError::Banned { scope });
                    }
                }
                Attempt::RateLimited { retry_after } => {
                    self.pool.release(&handle);
                    self.pool.record_rate_limited(&handle, retry_after);
                    let ra = retry_after.unwrap_or(RATE_LIMIT_BASE_SECS);
                    if ra > SHORT_RETRY_SECS || rate_tries >= 2 {
                        return Err(UpstreamError::RateLimited { retry_after: ra });
                    }
                    rate_tries += 1;
                    tokio::time::sleep(Duration::from_secs(ra.max(1))).await;
                }
                Attempt::Challenge => {
                    // 挑战失效：不换 egress、不记账，重取 VQD ≤3（§2.2 / §6.1）
                    self.pool.release(&handle);
                    self.vqd.invalidate();
                    let _ = self.vqd.note_challenge_failure();
                    challenge_tries += 1;
                    if challenge_tries >= MAX_CHALLENGE_TRIES || self.vqd.challenge_exhausted() {
                        return Err(UpstreamError::ChallengeFailed);
                    }
                }
            }
        }
    }

    async fn probe(&self) -> Result<(), UpstreamError> {
        // 首页缓存顺带刷新（30min TTL 内不实际请求）
        let _ = self.fe_meta().await;
        let handle = self
            .pool
            .acquire(None)
            .ok_or_else(|| UpstreamError::Transport("无可用 egress".into()))?;
        let client = self.client_for(&handle);
        let headers = header_map(&build_status_headers(&self.cfg.base));
        let url = format!("{}/duckchat/v1/status", self.cfg.base);
        let result = client
            .get(url)
            .headers(headers)
            .send()
            .await
            .map_err(|err| {
                if err.is_timeout() {
                    UpstreamError::Timeout
                } else {
                    UpstreamError::Transport(err.to_string())
                }
            });
        self.pool.release(&handle);
        let resp = result?;
        match resp.status().as_u16() {
            418 => {
                self.pool.record_banned(&handle, "418 ERR_BN_LIMIT (probe)");
                Err(UpstreamError::Banned {
                    scope: handle.scope,
                })
            }
            429 => {
                let ra = retry_after_secs(resp.headers());
                self.pool.record_rate_limited(&handle, ra);
                Err(UpstreamError::RateLimited {
                    retry_after: ra.unwrap_or(RATE_LIMIT_BASE_SECS),
                })
            }
            s if (200..300).contains(&s) => {
                self.pool.record_success(&handle);
                Ok(())
            }
            s => Err(UpstreamError::Upstream {
                status: s,
                body: "status probe".into(),
            }),
        }
    }
}

impl HttpUpstream {
    async fn fetch_models(&self) -> Option<Vec<ModelInfo>> {
        let handle = self.pool.acquire(None)?;
        let client = self.client_for(&handle);
        let mut headers = header_map(&build_status_headers(&self.cfg.base));
        headers.insert(
            reqwest::header::ACCEPT,
            reqwest::header::HeaderValue::from_static("application/json"),
        );
        let url = format!("{}/duckchat/v1/models", self.cfg.base);
        let fetched: Option<String> = match client.get(url).headers(headers).send().await {
            Ok(resp) if resp.status().is_success() => resp.text().await.ok(),
            _ => None,
        };
        self.pool.release(&handle);
        let text = fetched?;

        #[derive(Deserialize)]
        struct ModelsWire {
            #[serde(default)]
            models: Vec<ModelWire>,
        }
        #[derive(Deserialize)]
        struct ModelWire {
            #[serde(default)]
            id: String,
            #[serde(default)]
            provider: Option<String>,
        }

        let wire: ModelsWire = serde_json::from_str(&text).ok()?;
        if wire.models.is_empty() {
            return None;
        }
        // 上游 id 权威；别名从本地快照补（旧名不丢）
        let snapshot = ModelCatalog::snapshot();
        let mut out: Vec<ModelInfo> = wire
            .models
            .into_iter()
            .filter(|m| !m.id.is_empty())
            .map(|m| {
                let mut info =
                    ModelInfo::snapshot(m.id, m.provider.unwrap_or_else(|| "duck.ai".to_string()));
                info.source = ModelSource::Upstream;
                if let Some(snap) = snapshot.resolve(&info.id) {
                    info.aliases = snap.aliases.clone();
                }
                info
            })
            .collect();
        // 上游没返回的快照模型继续兜底（P1-8 精神：永远可用）
        for snap in snapshot.list() {
            if !out.iter().any(|m| m.id == snap.id) {
                out.push(snap.clone());
            }
        }
        Some(out)
    }
}

// ---------- 辅助 ----------

/// 流内联错误 → 分类错误（映射由 API 层按 §9 执行）。
pub(crate) fn classify_inline(
    status: Option<u16>,
    kind: &str,
    override_code: Option<&str>,
    scope: EgressScope,
) -> UpstreamError {
    let k = kind.to_ascii_uppercase();
    if k.contains("CHALLENGE") || k.contains("VQD") {
        UpstreamError::ChallengeFailed
    } else if k.contains("BN_LIMIT") || k.contains("BANNED") || k.contains("_BAN") {
        UpstreamError::Banned { scope }
    } else if k.contains("RATE") || k.contains("429") {
        UpstreamError::RateLimited {
            retry_after: SHORT_RETRY_SECS,
        }
    } else {
        let code = override_code
            .and_then(|c| c.parse::<u16>().ok())
            .or(status)
            .unwrap_or(502);
        UpstreamError::Upstream {
            status: code,
            body: kind.to_string(),
        }
    }
}

fn post_for(err: &UpstreamError) -> PostAction {
    match err {
        UpstreamError::Banned { .. } => PostAction::Banned("stream ERR_BN_LIMIT"),
        UpstreamError::RateLimited { retry_after } => PostAction::RateLimited(Some(*retry_after)),
        _ => PostAction::None,
    }
}

fn retry_after_secs(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
}

fn header_map(pairs: &[(String, String)]) -> reqwest::header::HeaderMap {
    let mut map = reqwest::header::HeaderMap::with_capacity(pairs.len());
    for (name, value) in pairs {
        if let (Ok(n), Ok(v)) = (
            reqwest::header::HeaderName::from_bytes(name.as_bytes()),
            reqwest::header::HeaderValue::from_str(value),
        ) {
            map.insert(n, v);
        }
    }
    map
}

fn truncate(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_challenge_maps_to_challenge_failed() {
        let e = classify_inline(Some(400), "ERR_CHALLENGE", None, EgressScope::Direct);
        assert!(matches!(e, UpstreamError::ChallengeFailed));
    }

    #[test]
    fn inline_ban_maps_to_banned_with_scope() {
        let e = classify_inline(Some(418), "ERR_BN_LIMIT", None, EgressScope::Proxy(1));
        match e {
            UpstreamError::Banned { scope } => assert_eq!(scope, EgressScope::Proxy(1)),
            other => panic!("期望 Banned，得到 {other:?}"),
        }
    }

    #[test]
    fn inline_rate_maps_to_rate_limited() {
        let e = classify_inline(Some(429), "ERR_RATE_LIMITED", None, EgressScope::Direct);
        assert!(matches!(e, UpstreamError::RateLimited { .. }));
    }

    #[test]
    fn truncate_is_char_safe() {
        assert_eq!(truncate("中文abc", 2), "中文");
        assert_eq!(truncate("short", 500), "short");
    }

    #[test]
    fn retry_after_parses_seconds_only() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "retry-after",
            reqwest::header::HeaderValue::from_static("7"),
        );
        assert_eq!(retry_after_secs(&headers), Some(7));
        let mut http_date = reqwest::header::HeaderMap::new();
        http_date.insert(
            "retry-after",
            reqwest::header::HeaderValue::from_static("Wed, 21 Oct 2026 07:28:00 GMT"),
        );
        assert_eq!(retry_after_secs(&http_date), None);
    }
}
