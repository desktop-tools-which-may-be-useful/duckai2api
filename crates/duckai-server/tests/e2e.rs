//! 装配层集成测试：完整应用（配置 → 工厂 → 路由）打真实 Router。
//! 上游用 wiremock 模拟（复用 fixtures），**永不访问真实网络**。

use axum::body::Body;
use axum::http::{Request, Response, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;
use wiremock::matchers::{header_exists, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use duckai_server::ServerConfig;

const HOME_HTML: &str = include_str!("../../../fixtures/home.html");
const MODELS_JSON: &str = include_str!("../../../fixtures/models.json");
const SSE_SUCCESS: &str = include_str!("../../../fixtures/sse_success.txt");
const CHALLENGE_B64: &str = include_str!("../../../fixtures/challenge_fresh.b64");

/// 装配测试配置：回环 + 默认 API key + 内存库（不落盘、不进 git）。
fn cfg_for(base: &str, extra: &[(&str, &str)]) -> ServerConfig {
    ServerConfig::from_lookup(&move |k: &str| match k {
        "DUCKAI_BASE" => Some(base.to_string()),
        "DUCKAI_DEFAULT_API_KEY" => Some("sk-test-key".to_string()),
        "DUCKAI_DB_PATH" => Some(":memory:".to_string()),
        _ => extra
            .iter()
            .find(|(ek, _)| *ek == k)
            .map(|(_, ev)| ev.to_string()),
    })
    .expect("测试配置合法")
}

async fn mount_upstream(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html")
                .set_body_string(HOME_HTML),
        )
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/duckchat/v1/status"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .insert_header("x-vqd-hash-1", CHALLENGE_B64.trim())
                .set_body_string("{}"),
        )
        .mount(server)
        .await;
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
    Mock::given(method("GET"))
        .and(path("/duckchat/v1/models"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(MODELS_JSON),
        )
        .mount(server)
        .await;
}

fn app(base: &str) -> duckai_server::App {
    duckai_server::build(cfg_for(base, &[])).expect("装配")
}

async fn call(app: &duckai_server::App, req: Request<Body>) -> Response<Body> {
    app.router.clone().oneshot(req).await.expect("oneshot")
}

async fn body_string(res: Response<Body>) -> String {
    let bytes = res.into_body().collect().await.expect("body").to_bytes();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn post(path: &str, bearer: Option<&str>, body: &str) -> Request<Body> {
    let mut req = Request::post(path).header("content-type", "application/json");
    if let Some(k) = bearer {
        req = req.header("authorization", format!("Bearer {k}"));
    }
    req.body(Body::from(body.to_string())).expect("request")
}

/// S2 契约：/health 免鉴权、形状固定、数据来自实况。
#[tokio::test]
async fn health_is_open_and_truthful() {
    let server = MockServer::start().await;
    mount_upstream(&server).await;
    let app = app(&server.uri());

    let res = call(&app, Request::get("/health").body(Body::empty()).unwrap()).await;
    assert_eq!(res.status(), StatusCode::OK, "/health 必须免鉴权");
    let v: serde_json::Value = serde_json::from_str(&body_string(res).await).unwrap();
    assert_eq!(v["status"], "ok");
    assert_eq!(v["upstream"]["mode"], "http");
    assert_eq!(v["egress"]["total"], 1, "默认单出口直连");
    assert!(v["inflight"].is_u64(), "在途计数存在");
}

/// Bearer 鉴权：缺失/错误 → 401，且是 JSON 错误帧（非裸文本/500）。
#[tokio::test]
async fn v1_requires_bearer() {
    let server = MockServer::start().await;
    mount_upstream(&server).await;
    let app = app(&server.uri());

    for bearer in [None, Some("sk-wrong")] {
        let res = call(
            &app,
            post(
                "/v1/chat/completions",
                bearer,
                r#"{"model":"gpt-5.6-luna","messages":[{"role":"user","content":"hi"}]}"#,
            ),
        )
        .await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "bearer={bearer:?}");
        let ct = res.headers().get("content-type").unwrap().to_str().unwrap();
        assert!(ct.contains("application/json"), "错误帧 content-type={ct}");
    }
}

/// 三协议非流式全通 + completion id 族一致性（chatcmpl-）。
#[tokio::test]
async fn three_protocols_nonstream_end_to_end() {
    let server = MockServer::start().await;
    mount_upstream(&server).await;
    let app = app(&server.uri());

    // OpenAI
    let res = call(
        &app,
        post(
            "/v1/chat/completions",
            Some("sk-test-key"),
            r#"{"model":"gpt-5.6-luna","messages":[{"role":"user","content":"hi"}]}"#,
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK, "chat completions");
    let v: serde_json::Value = serde_json::from_str(&body_string(res).await).unwrap();
    assert!(
        v["id"].as_str().unwrap_or("").starts_with("chatcmpl-"),
        "OpenAI id 族：{}",
        v["id"]
    );
    assert_eq!(v["model"], "gpt-5.6-luna");
    assert_eq!(v["object"], "chat.completion");
    let content = v["choices"][0]["message"]["content"].as_str().unwrap_or("");
    assert!(content.contains("PONG"), "上游 mock 文本透传：{content}");

    // Anthropic
    let res = call(
        &app,
        post(
            "/v1/messages",
            Some("sk-test-key"),
            r#"{"model":"gpt-5.6-luna","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}"#,
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK, "messages");
    let v: serde_json::Value = serde_json::from_str(&body_string(res).await).unwrap();
    assert!(
        v["id"].as_str().unwrap_or("").starts_with("msg_"),
        "Anthropic id 族：{}",
        v["id"]
    );
    assert_eq!(v["type"], "message");

    // Responses（原项目恒 500 的回归位）
    let res = call(
        &app,
        post(
            "/v1/responses",
            Some("sk-test-key"),
            r#"{"model":"gpt-5.6-luna","input":"hi"}"#,
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK, "responses 非流式");
    let v: serde_json::Value = serde_json::from_str(&body_string(res).await).unwrap();
    assert!(
        v["id"].as_str().unwrap_or("").starts_with("resp_"),
        "Responses id 族：{}",
        v["id"]
    );
}

/// OpenAI SSE 流式：chatcmpl- id + [DONE] 终止。
#[tokio::test]
async fn openai_stream_sse() {
    let server = MockServer::start().await;
    mount_upstream(&server).await;
    let app = app(&server.uri());

    let res = call(
        &app,
        post(
            "/v1/chat/completions",
            Some("sk-test-key"),
            r#"{"model":"gpt-5.6-luna","stream":true,"messages":[{"role":"user","content":"hi"}]}"#,
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let ct = res
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(ct.contains("text/event-stream"), "SSE 头：{ct}");
    let text = body_string(res).await;
    assert!(text.contains("chatcmpl-"), "流内 completion id 一致");
    assert!(text.contains("data: [DONE]"), "SSE 以 [DONE] 收尾");
}

/// /v1/models：上游拉取 + 别名透出（P1-8）。
#[tokio::test]
async fn models_endpoint_serves_upstream_list() {
    let server = MockServer::start().await;
    mount_upstream(&server).await;
    let app = app(&server.uri());

    let res = call(
        &app,
        Request::get("/v1/models")
            .header("authorization", "Bearer sk-test-key")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&body_string(res).await).unwrap();
    assert_eq!(v["object"], "list");
    assert!(
        v["data"].as_array().map(|a| !a.is_empty()).unwrap_or(false),
        "模型非空：{v}"
    );
}

/// 管理面端到端：口令登录 → cookie → 设置**实时**生效到后续请求装配。
/// （管理 API 随 `webui` feature 注册；feature 关闭时 `/admin/api/*` 为 404，属设计行为。）
#[cfg(feature = "webui")]
#[tokio::test]
async fn admin_login_and_live_settings() {
    let server = MockServer::start().await;
    mount_upstream(&server).await;
    let app = duckai_server::build(cfg_for(
        &server.uri(),
        &[("DUCKAI_ADMIN_PASSWORD", "s3cret")],
    ))
    .expect("装配");

    // 无 cookie → 401
    let res = call(
        &app,
        Request::get("/admin/api/status")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "管理面必须带会话");

    // 错口令 → 401
    let res = call(
        &app,
        Request::post("/admin/api/login")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"password":"nope"}"#))
            .unwrap(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

    // 正确口令 → Set-Cookie
    let res = call(
        &app,
        Request::post("/admin/api/login")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"password":"s3cret"}"#))
            .unwrap(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK, "登录成功");
    let cookie = res
        .headers()
        .get("set-cookie")
        .expect("下发会话 cookie")
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();

    // 带 cookie 读状态
    let res = call(
        &app,
        Request::get("/admin/api/status")
            .header("cookie", &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&body_string(res).await).unwrap();
    assert_eq!(v["settings"]["default_model"], "gpt-5.6-luna");
    assert_eq!(v["settings"]["max_concurrency"], 8);
    assert_eq!(v["settings"]["auth_enabled"], true);
    assert_eq!(v["settings"]["admin_password_set"], true);
    assert_eq!(v["settings"]["upstream_mode"], "http");
    assert_eq!(v["health"]["egress"]["total"], 1);

    // 改默认模型 → 状态立刻反映（活配置）
    let res = call(
        &app,
        Request::post("/admin/api/settings")
            .header("content-type", "application/json")
            .header("cookie", &cookie)
            .body(Body::from(
                r#"{"default_model":"gpt-5.6-terra","max_concurrency":4}"#,
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK, "设置保存");
    let res = call(
        &app,
        Request::get("/admin/api/status")
            .header("cookie", &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&body_string(res).await).unwrap();
    assert_eq!(
        v["settings"]["default_model"], "gpt-5.6-terra",
        "写锁实时可见"
    );
    assert_eq!(v["settings"]["max_concurrency"], 4, "闸门实时可见");

    // 之后的 OpenAI 请求缺省 model 走新默认值
    let res = call(
        &app,
        post(
            "/v1/chat/completions",
            Some("sk-test-key"),
            r#"{"messages":[{"role":"user","content":"hi"}]}"#,
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK, "省略 model 回落新默认");
    let v: serde_json::Value = serde_json::from_str(&body_string(res).await).unwrap();
    assert_eq!(v["model"], "gpt-5.6-terra");
}

/// C2 管理 API：密钥 CRUD / 出口策略 / 直连开关 / 设置保存 / 改口令，全部写穿。
#[cfg(feature = "webui")]
#[tokio::test]
async fn admin_config_api_write_through() {
    let server = MockServer::start().await;
    mount_upstream(&server).await;
    let app = duckai_server::build(cfg_for(
        &server.uri(),
        &[("DUCKAI_ADMIN_PASSWORD", "s3cret")],
    ))
    .expect("装配");

    // 登录取会话（复用首测路径）
    let res = call(
        &app,
        Request::post("/admin/api/login")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"password":"s3cret"}"#))
            .unwrap(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK, "登录成功");
    let cookie = res
        .headers()
        .get("set-cookie")
        .expect("会话 cookie")
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();

    let api = |method: axum::http::Method, path: &str, body: &str| {
        let req = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .header("cookie", &cookie);
        req.body(Body::from(body.to_string())).unwrap()
    };

    // ---- 密钥 CRUD ----
    let res = call(&app, api(axum::http::Method::GET, "/admin/api/keys", "")).await;
    assert_eq!(res.status(), StatusCode::OK, "keys 列表");
    let v: serde_json::Value = serde_json::from_str(&body_string(res).await).unwrap();
    assert_eq!(v["keys"].as_array().unwrap().len(), 1, "env 引导 key");

    let res = call(
        &app,
        api(
            axum::http::Method::POST,
            "/admin/api/keys",
            r#"{"label":"e2e"}"#,
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK, "建 key");
    let v: serde_json::Value = serde_json::from_str(&body_string(res).await).unwrap();
    let new_key = v["key"].as_str().unwrap().to_string();
    assert!(new_key.starts_with("sk-"), "一次性明文：{new_key}");
    let kid = v["keys"]
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["label"] == "e2e")
        .expect("在列")["id"]
        .as_i64()
        .unwrap();

    let res = call(
        &app,
        api(
            axum::http::Method::DELETE,
            &format!("/admin/api/keys/{kid}"),
            "",
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK, "吊销");

    // ---- 出口策略（per-egress） ----
    let res = call(
        &app,
        api(
            axum::http::Method::POST,
            "/admin/api/egress/policy",
            r#"{"index":0,"policy":{"enabled":true,"cooldown_enabled":false,"ban_secs":42,"ban_cap_secs":3600,"rate_limit_secs":9}}"#,
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK, "设策略");
    let v: serde_json::Value = serde_json::from_str(&body_string(res).await).unwrap();
    assert_eq!(v["egresses"][0]["ban_secs"], 42);
    assert_eq!(
        v["egresses"][0]["cooldown_enabled"],
        serde_json::json!(false)
    );

    // 非法策略 400（不落库）
    let res = call(
        &app,
        api(
            axum::http::Method::POST,
            "/admin/api/egress/policy",
            r#"{"index":0,"policy":{"enabled":true,"cooldown_enabled":true,"ban_secs":0,"ban_cap_secs":86400,"rate_limit_secs":5}}"#,
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST, "ban_secs=0 拒绝");

    // ---- 直连开关（先加代理，保留最后一个出口规则） ----
    let res = call(
        &app,
        api(
            axum::http::Method::POST,
            "/admin/api/proxy",
            r#"{"url":"http://127.0.0.1:1080"}"#,
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK, "加代理");
    let res = call(
        &app,
        api(
            axum::http::Method::POST,
            "/admin/api/egress/direct",
            r#"{"enabled":false}"#,
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK, "关直连");
    let v: serde_json::Value = serde_json::from_str(&body_string(res).await).unwrap();
    assert!(
        v["egresses"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["direct"].as_bool() == Some(false)),
        "直连已关：{}",
        v["egresses"]
    );

    // ---- 设置保存（base 尾斜杠修剪 + 生效说明） ----
    let res = call(
        &app,
        api(
            axum::http::Method::POST,
            "/admin/api/settings/save",
            r#"{"base":"https://new.example/","vqd_override":"","new_chat":true}"#,
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK, "设置保存");
    let v: serde_json::Value = serde_json::from_str(&body_string(res).await).unwrap();
    assert_eq!(v["settings"]["base"], "https://new.example");
    assert_eq!(v["settings"]["new_chat"], serde_json::json!(true));

    // ---- 改口令：旧口令失效、新口令生效 ----
    let res = call(
        &app,
        api(
            axum::http::Method::POST,
            "/admin/api/password",
            r#"{"old":"s3cret","new":"newer-pass"}"#,
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK, "改口令");
    let res = call(
        &app,
        Request::post("/admin/api/login")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"password":"s3cret"}"#))
            .unwrap(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "旧口令已失效");
    let res = call(
        &app,
        Request::post("/admin/api/login")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"password":"newer-pass"}"#))
            .unwrap(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK, "新口令可登录");
}

/// WebUI 静态层：控制台页与两份资源可达（§8 静态覆盖实现）。
#[cfg(feature = "webui")]
#[tokio::test]
async fn webui_static_assets_served() {
    let server = MockServer::start().await;
    let app = app(&server.uri());

    let res = call(&app, Request::get("/").body(Body::empty()).unwrap()).await;
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_string(res).await;
    assert!(html.contains("duckai2api-rust"), "控制台标题");
    assert!(html.contains("/ui/app.js"), "无构建链的两份静态资源之一");

    for p in ["/ui/app.js", "/index.html"] {
        let res = call(&app, Request::get(p).body(Body::empty()).unwrap()).await;
        assert_eq!(res.status(), StatusCode::OK, "{p} 可达");
    }
}
