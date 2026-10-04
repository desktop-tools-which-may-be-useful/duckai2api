//! HTTP 适配器集成测试（wiremock 模拟上游；**永不访问真实网络**）。
//!
//! 覆盖：§2.2 VQD 求解链、SSE 事件映射、418→Banned→解封恢复（P0-3/P1-6）、
//! 429 尊重 Retry-After、挑战错误 ≤3 重取且不消耗 egress、模型列表、探活、传输失败。

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures::StreamExt;
use wiremock::matchers::{header_exists, method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use duckai_types::{ChatTurn, Role, UpstreamError, UpstreamEvent, UpstreamRequest};
use duckai_upstream::{
    EgressPool, FactoryConfig, HttpUpstream, UpstreamClient, UpstreamRequest as _ReexportCheck,
};

const HOME_HTML: &str = include_str!("../../../fixtures/home.html");
const MODELS_JSON: &str = include_str!("../../../fixtures/models.json");
const SSE_SUCCESS: &str = include_str!("../../../fixtures/sse_success.txt");
const CHALLENGE_B64: &str = include_str!("../../../fixtures/challenge_fresh.b64");

fn req() -> UpstreamRequest {
    UpstreamRequest::new("gpt-5.6-luna", vec![ChatTurn::text(Role::User, "你好")])
}

async fn mount_home(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html")
                .set_body_string(HOME_HTML),
        )
        .mount(server)
        .await;
}

async fn mount_status_ok(server: &MockServer) {
    let challenge = CHALLENGE_B64.trim();
    Mock::given(method("GET"))
        .and(path("/duckchat/v1/status"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .insert_header("x-vqd-hash-1", challenge)
                .set_body_string("{}"),
        )
        .mount(server)
        .await;
}

async fn mount_chat_success(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/duckchat/v1/chat"))
        .and(header_exists("x-fe-version"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(SSE_SUCCESS),
        )
        .mount(server)
        .await;
}

fn upstream(server: &MockServer) -> Arc<HttpUpstream> {
    let cfg = FactoryConfig {
        base: server.uri(),
        ..FactoryConfig::default()
    };
    let pool = Arc::new(EgressPool::new(&[]).expect("direct pool"));
    Arc::new(HttpUpstream::new(cfg, pool).expect("http upstream"))
}

async fn collect(client: &HttpUpstream) -> Result<Vec<UpstreamEvent>, UpstreamError> {
    let stream = client.chat(req()).await?;
    let mut out = Vec::new();
    let mut stream = stream;
    while let Some(item) = stream.next().await {
        out.push(item?);
    }
    Ok(out)
}

/// 正常路径：VQD 求解（纯 Rust PoW）→ 17 头 POST → SSE 逐帧映射 → Done。
#[tokio::test]
async fn chat_solves_vqd_and_streams_frames() {
    let server = MockServer::start().await;
    mount_home(&server).await;
    mount_status_ok(&server).await;
    mount_chat_success(&server).await;

    let client = upstream(&server);
    let events = collect(&client).await.expect("chat ok");

    let texts: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            UpstreamEvent::TextDelta(s) => Some(s.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, vec!["P", "ONG"]);
    assert!(
        matches!(events.last(), Some(UpstreamEvent::Done { finish_reason }) if finish_reason == "stop"),
        "应以 Done{{stop}} 收尾：{events:?}"
    );

    // POST 载荷与请求头
    let received = server.received_requests().await.expect("requests");
    let posts: Vec<&Request> = received
        .iter()
        .filter(|r| r.method == "POST" && r.url.path() == "/duckchat/v1/chat")
        .collect();
    assert_eq!(posts.len(), 1, "chat POST 应恰好一次");
    let body = String::from_utf8_lossy(&posts[0].body);
    assert!(
        body.contains("durableStream"),
        "应含 durableStream: {body:.200}"
    );
    assert!(body.contains("\"model\""), "应含 model 字段");
    let has_fe = posts[0]
        .headers
        .get("x-fe-version")
        .map(|_| true)
        .unwrap_or(false);
    assert!(has_fe, "chat 应带 x-fe-version 头");

    // VQD 应已缓存（观测位）
    let (valid, _) = client.observability();
    assert!(valid, "求解后 VQD 应有效");
}

/// 418 → Banned → 立即拒绝（不发请求）→ admin 解封后恢复（P0-3 单出口版 + P1-6）。
#[tokio::test]
async fn ban_418_then_unban_recovers() {
    let server = MockServer::start().await;
    mount_home(&server).await;
    mount_status_ok(&server).await;
    Mock::given(method("POST"))
        .and(path("/duckchat/v1/chat"))
        .respond_with(
            ResponseTemplate::new(418)
                .insert_header("content-type", "application/json")
                .set_body_string(r#"{"action":"error","status":418,"type":"ERR_BN_LIMIT"}"#),
        )
        .mount(&server)
        .await;

    let client = upstream(&server);
    let err = collect(&client).await.expect_err("418 应失败");
    assert!(
        matches!(err, UpstreamError::Banned { .. }),
        "418 应映射 Banned，实际 {err:?}"
    );
    assert!(
        !err.to_string().contains("账号被封"),
        "Banned 文案不得出现「账号被封」"
    );

    // 单出口被封 → 再次调用应直接拒绝，不再产生 HTTP 流量
    let before = server.received_requests().await.unwrap().len();
    let err2 = collect(&client).await.expect_err("banned 后应直接失败");
    assert!(matches!(err2, UpstreamError::Banned { .. }));
    let after = server.received_requests().await.unwrap().len();
    assert_eq!(before, after, "Banned 期间不应发起任何上游请求");

    // admin 解封 → 恢复健康 → 换 200 模型继续服务
    client.pool().admin_unban(0).expect("unban");
    Mock::given(method("POST"))
        .and(path("/duckchat/v1/chat"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(SSE_SUCCESS),
        )
        .mount(&server)
        .await;
    // wiremock 后挂的 mock 不会覆盖先前 418——用优先级顶掉
    server.reset().await;
    mount_home(&server).await;
    mount_status_ok(&server).await;
    mount_chat_success(&server).await;
    let events = collect(&client).await.expect("解封后应恢复");
    assert!(events.iter().any(|e| e.is_done()));
}

/// 429 + Retry-After: 1s → 尊重退避后重试成功（限流而非裸 500）。
#[tokio::test]
async fn rate_limit_honors_retry_after_then_recovers() {
    struct Flaky {
        calls: Arc<AtomicUsize>,
    }
    impl Respond for Flaky {
        fn respond(&self, _req: &Request) -> ResponseTemplate {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                ResponseTemplate::new(429).insert_header("retry-after", "1")
            } else {
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(SSE_SUCCESS)
            }
        }
    }

    let server = MockServer::start().await;
    mount_home(&server).await;
    mount_status_ok(&server).await;
    let calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/duckchat/v1/chat"))
        .respond_with(Flaky {
            calls: calls.clone(),
        })
        .mount(&server)
        .await;

    let client = upstream(&server);
    let started = std::time::Instant::now();
    let events = collect(&client).await.expect("429 后重试应成功");
    assert!(events.iter().any(|e| e.is_done()));
    assert!(
        started.elapsed() >= Duration::from_millis(900),
        "应等待 Retry-After"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2, "应恰好两次 POST");
}

/// 挑战错误：≤3 次重取后返回 ChallengeFailed（503），且不切换/消耗 egress。
#[tokio::test]
async fn challenge_error_refetches_three_then_fails() {
    let server = MockServer::start().await;
    mount_home(&server).await;
    mount_status_ok(&server).await;
    Mock::given(method("POST"))
        .and(path("/duckchat/v1/chat"))
        .respond_with(
            ResponseTemplate::new(400)
                .insert_header("content-type", "application/json")
                .set_body_string(r#"{"action":"error","status":400,"type":"ERR_CHALLENGE"}"#),
        )
        .mount(&server)
        .await;

    let client = upstream(&server);
    let err = collect(&client).await.expect_err("挑战失败应报错");
    assert!(
        matches!(err, UpstreamError::ChallengeFailed),
        "应为 ChallengeFailed，实际 {err:?}"
    );

    let received = server.received_requests().await.unwrap();
    let posts = received
        .iter()
        .filter(|r| r.method == "POST" && r.url.path() == "/duckchat/v1/chat")
        .count();
    let gets = received
        .iter()
        .filter(|r| r.method == "GET" && r.url.path() == "/duckchat/v1/status")
        .count();
    assert_eq!(posts, 3, "挑战重试 ≤3 次");
    assert_eq!(gets, 3, "每次重试应重新取挑战（≤3+1 初始共 ≤3 次求解）");
    // 挑战错误不消耗 egress：仍是健康可调度
    assert_eq!(client.pool().healthy_count(), 1, "挑战错误不得惩罚 egress");
}

/// 模型列表：上游拉取 + 快照别名合并 + 30 分钟缓存。
#[tokio::test]
async fn list_models_fetches_once_and_merges_aliases() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/duckchat/v1/models"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(MODELS_JSON),
        )
        .mount(&server)
        .await;

    let client = upstream(&server);
    let models = client.list_models().await.expect("models ok");
    assert!(!models.is_empty(), "上游模型非空");

    let _ = client.list_models().await.expect("models ok (cached)");
    let received = server.received_requests().await.unwrap();
    let hits = received
        .iter()
        .filter(|r| r.url.path() == "/duckchat/v1/models")
        .count();
    assert_eq!(hits, 1, "第二次应命中 30 分钟缓存");
}

/// probe()：状态 200 → Ok；418 → Banned。
#[tokio::test]
async fn probe_maps_status_codes() {
    let server = MockServer::start().await;
    mount_home(&server).await;
    mount_status_ok(&server).await;
    let client = upstream(&server);
    client.probe().await.expect("probe ok");

    let server2 = MockServer::start().await;
    mount_home(&server2).await;
    Mock::given(method("GET"))
        .and(path("/duckchat/v1/status"))
        .respond_with(ResponseTemplate::new(418).set_body_string("banned"))
        .mount(&server2)
        .await;
    let client2 = upstream(&server2);
    let err = client2.probe().await.expect_err("418 probe 应失败");
    assert!(matches!(err, UpstreamError::Banned { .. }), "{err:?}");
}

/// 传输失败（连接拒绝）→ Transport，而非 ChallengeFailed 或裸 panic。
#[tokio::test]
async fn transport_error_maps_to_transport() {
    let cfg = FactoryConfig {
        base: "http://127.0.0.1:9".into(), // 丢弃端口，必然拒绝
        ..FactoryConfig::default()
    };
    let pool = Arc::new(EgressPool::new(&[]).unwrap());
    let client = HttpUpstream::new(cfg, pool).unwrap();
    let err = collect(&client).await.expect_err("连接失败应报错");
    assert!(
        matches!(err, UpstreamError::Transport(_) | UpstreamError::Timeout),
        "应映射 Transport/Timeout，实际 {err:?}"
    );
}

// 占位：确认 `_ReexportCheck` 类型别名可编译（UpstreamRequest 从 upstream 再导出）。
#[allow(dead_code)]
type _AssertReexport = dyn FnOnce() -> _ReexportCheck;

/// P0-3 核心断言：418 → 代理 A 被封 → 自动切换代理 B → 第二次请求成功。
///
/// 用两个极简正向代理（absolute-form HTTP 转发，无 CONNECT）把请求接到同一
/// wiremock 后端；首次 chat POST 418 封禁 A，重试必须改走 B（hits_b≥1）。
#[tokio::test]
async fn ban_418_switches_proxy_and_second_succeeds() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    /// 正向代理：读取 absolute-form 请求 → 改写为 origin-form → 转发到真实
    /// 后端并双向搬运响应；`Connection: close` 保证事务有界。
    async fn forward_proxy(hits: Arc<AtomicUsize>) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind proxy");
        let addr = listener.local_addr().expect("proxy addr");
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let hits = hits.clone();
                tokio::spawn(async move {
                    let _ =
                        tokio::time::timeout(Duration::from_secs(10), proxy_one(&mut sock, hits))
                            .await;
                });
            }
        });
        addr
    }

    async fn proxy_one(sock: &mut TcpStream, hits: Arc<AtomicUsize>) -> std::io::Result<()> {
        // 读请求头
        let mut buf: Vec<u8> = Vec::new();
        let mut tmp = [0u8; 4096];
        let head_end = loop {
            let n = sock.read(&mut tmp).await?;
            if n == 0 {
                return Ok(());
            }
            buf.extend_from_slice(&tmp[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
            if buf.len() > 64 * 1024 {
                return Ok(());
            }
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
        let mut lines = head.split("\r\n");
        let mut req_parts = lines.next().unwrap_or_default().split(' ');
        let method = req_parts.next().unwrap_or("GET").to_string();
        let target = req_parts.next().unwrap_or("/").to_string();

        let mut content_len: usize = 0;
        let mut keep_headers: Vec<String> = Vec::new();
        for line in lines {
            if line.is_empty() {
                continue;
            }
            let lower = line.to_ascii_lowercase();
            if let Some(v) = lower.strip_prefix("content-length:") {
                content_len = v.trim().parse().unwrap_or(0);
            }
            if lower.starts_with("proxy-connection:") || lower.starts_with("connection:") {
                continue;
            }
            keep_headers.push(line.to_string());
        }

        // 读正文
        let mut body = buf[head_end..].to_vec();
        while body.len() < content_len {
            let n = sock.read(&mut tmp).await?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&tmp[..n]);
        }

        // absolute-form → origin-form + 上游地址
        let authority = if let Some(rest) = target.strip_prefix("http://") {
            match rest.find('/') {
                Some(i) => rest[..i].to_string(),
                None => rest.to_string(),
            }
        } else {
            keep_headers
                .iter()
                .find_map(|h| {
                    let lower = h.to_ascii_lowercase();
                    lower.strip_prefix("host:").map(|v| v.trim().to_string())
                })
                .unwrap_or_default()
        };
        let path = if let Some(rest) = target.strip_prefix("http://") {
            match rest.find('/') {
                Some(i) => rest[i..].to_string(),
                None => "/".to_string(),
            }
        } else {
            target.clone()
        };

        hits.fetch_add(1, Ordering::SeqCst);
        let mut upstream = TcpStream::connect(&authority).await?;
        let mut fwd = format!("{method} {path} HTTP/1.1\r\n");
        for h in &keep_headers {
            fwd.push_str(h);
            fwd.push_str("\r\n");
        }
        fwd.push_str("connection: close\r\n\r\n");
        upstream.write_all(fwd.as_bytes()).await?;
        upstream.write_all(&body).await?;

        let (from_client, to_client) = sock.split();
        let (mut r_up, mut w_up) = upstream.into_split();
        // 后端→客户端
        let c2u = async {
            let mut from_client = from_client;
            let _ = tokio::io::copy(&mut from_client, &mut w_up).await;
        };
        // 客户端→后端 …不对：上面是客户端→后端；下面后端→客户端
        let u2c = async {
            let mut to_client = to_client;
            let _ = tokio::io::copy(&mut r_up, &mut to_client).await;
        };
        tokio::select! {
            _ = c2u => {}
            _ = u2c => {}
        }
        Ok(())
    }

    // 后端：首次 chat 418，之后 200
    struct BanThenOk {
        calls: Arc<AtomicUsize>,
    }
    impl Respond for BanThenOk {
        fn respond(&self, _req: &Request) -> ResponseTemplate {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                ResponseTemplate::new(418)
                    .insert_header("content-type", "application/json")
                    .set_body_string(r#"{"action":"error","status":418,"type":"ERR_BN_LIMIT"}"#)
            } else {
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(SSE_SUCCESS)
            }
        }
    }

    let backend = MockServer::start().await;
    mount_home(&backend).await;
    mount_status_ok(&backend).await;
    let chat_calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/duckchat/v1/chat"))
        .respond_with(BanThenOk {
            calls: chat_calls.clone(),
        })
        .mount(&backend)
        .await;

    let hits_a = Arc::new(AtomicUsize::new(0));
    let hits_b = Arc::new(AtomicUsize::new(0));
    let addr_a = forward_proxy(hits_a.clone()).await;
    let addr_b = forward_proxy(hits_b.clone()).await;

    let cfg = FactoryConfig {
        base: backend.uri(),
        proxies: vec![format!("http://{addr_a}"), format!("http://{addr_b}")],
        ..FactoryConfig::default()
    };
    let pool = Arc::new(EgressPool::new(&cfg.proxies).expect("two proxy slots"));
    let client = HttpUpstream::new(cfg, pool).expect("http upstream");

    let events = collect(&client).await.expect("切换代理后应成功");
    assert!(events.iter().any(|e| e.is_done()));

    // 槽位 A 实际被用过并被封；重试必须改走 B
    assert!(hits_a.load(Ordering::SeqCst) >= 1, "代理 A 应承载首次请求");
    assert!(
        hits_b.load(Ordering::SeqCst) >= 1,
        "418 后必须切换到代理 B（P0-3 回归）"
    );
    assert_eq!(
        chat_calls.load(Ordering::SeqCst),
        2,
        "后端应看到恰好两次 chat POST"
    );

    // 状态机断言：A banned、B healthy
    let snap = client.pool().snapshot();
    assert_eq!(snap.len(), 2);
    assert!(snap[0].state == "Banned", "槽位 A 应为 Banned：{snap:?}");
    assert!(snap[1].state == "Healthy", "槽位 B 应为 Healthy：{snap:?}");
}
