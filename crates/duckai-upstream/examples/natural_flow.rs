//! 决定性实验：在**本机出口 IP** 上驱动真实 duck.ai 页面自然聊天，
//! 用 CDP 抓取原生 status/chat 的线上 wire（请求头 + 实际发送头 + 响应状态/头），
//! 作为“完美模仿”基线，回答：从我们的 IP，真实页面流程能否通过。
//!
//! 运行：
//! ```sh
//! DUCKAI_CHROME_PATH=<chrome> cargo run -p duckai-upstream --example natural_flow \
//!     --features browser --release
//! ```
//! 输出：每条 `WIRE {json}` 一行（url/method/请求头/响应状态/实际线上头/响应头），
//! 以及关键状态行 `STEP ...`。最终截图 `/tmp/natural-flow.png`。

use std::time::{Duration, Instant};

use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::input::{
    DispatchKeyEventParams, DispatchKeyEventType, InsertTextParams,
};
use chromiumoxide::cdp::browser_protocol::network::{
    EventRequestWillBeSent, EventResponseReceived,
};
use chromiumoxide::page::ScreenshotParams;
use futures::StreamExt;

const DUCK: &str = "https://duck.ai/";
const MSG: &str = "仅回复ok";
const NEEDLE_CHAT: &str = "/duckchat/v1/chat";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut builder = BrowserConfig::builder()
        .with_head()
        .arg("disable-blink-features=AutomationControlled")
        .arg("no-first-run")
        .arg(format!(
            "user-agent={}",
            duckai_protocol::headers::USER_AGENT
        ));
    if let Ok(path) = std::env::var("DUCKAI_CHROME_PATH") {
        builder = builder.chrome_executable(path);
    }
    let config = builder.build()?;
    let (mut browser, mut handler) = Browser::launch(config).await?;
    tokio::spawn(async move {
        while let Some(res) = handler.next().await {
            if res.is_err() {
                break;
            }
        }
    });

    let page = browser.new_page("about:blank").await?;
    // 先挂监听再导航，保证 status/chat 全量入网。
    let mut reqs = page
        .event_listener::<EventRequestWillBeSent>()
        .await?
        .fuse();
    let mut resps = page.event_listener::<EventResponseReceived>().await?.fuse();

    let (tx, mut rx) = tokio::sync::mpsc::channel::<serde_json::Value>(512);
    tokio::spawn(async move {
        loop {
            tokio::select! {
                Some(ev) = reqs.next() => {
                    let req = &ev.request;
                    if req.url.contains("/duckchat/") || req.url.contains("/duck.ai/api/") {
                        let _ = tx.send(serde_json::json!({
                            "kind": "req",
                            "request_id": format!("{:?}", ev.request_id),
                            "url": req.url,
                            "method": req.method,
                            "headers": req.headers.inner(),
                        })).await;
                    }
                }
                Some(ev) = resps.next() => {
                    let r = &ev.response;
                    if r.url.contains("/duckchat/") || r.url.contains("/duck.ai/api/") {
                        let _ = tx.send(serde_json::json!({
                            "kind": "resp",
                            "request_id": format!("{:?}", ev.request_id),
                            "url": r.url,
                            "status": r.status,
                            "wire_request_headers": r.request_headers.as_ref().map(|h| h.inner()).cloned(),
                            "response_headers": r.headers.inner(),
                        })).await;
                    }
                }
                else => break,
            }
        }
    });

    // —— 导航到真实首页 ——
    println!("STEP goto {DUCK}");
    page.goto(DUCK).await?;
    page.wait_for_navigation().await?;

    // —— 水合 + 引导处理（有明显按钮就点一次）——
    let mut input_ready: Option<String> = None;
    let deadline = Instant::now() + Duration::from_secs(45);
    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let probe = page
            .evaluate(
                r#"(() => {
                    const el = document.querySelector('textarea, [contenteditable="true"], [role="textbox"], [data-slate-editor="true"], .cm-content');
                    const btns = Array.from(document.querySelectorAll('button')).map(b => (b.innerText || '').trim()).filter(Boolean).slice(0, 12);
                    const txt = (document.body.innerText || '').replace(/\s+/g, ' ').slice(0, 240);
                    return JSON.stringify({ hasInput: !!el, btns, txt });
                })()"#,
            )
            .await?
            .into_value::<String>()?;
        let v: serde_json::Value = serde_json::from_str(&probe)?;
        println!("STEP probe {}", serde_json::to_string(&v)?);
        if v["hasInput"].as_bool() == Some(true) {
            input_ready = Some("ready".into());
            break;
        }
        // 引导弹层：点击启动/同意类按钮一次。
        if let Some(btns) = v["btns"].as_array() {
            for b in btns {
                let t = b.as_str().unwrap_or("");
                if regexish_start(t) {
                    let clicked: bool = page
                        .evaluate(
                            r#"(() => {
                                const list = Array.from(document.querySelectorAll('button'));
                                const b = list.find(x => /start|开始|继续|continue|同意|accept|免费|new chat/i.test(x.innerText || ''));
                                if (b) { b.click(); return true; }
                                return false;
                            })()"#,
                        )
                        .await?
                        .into_value::<bool>()?;
                    println!("STEP clicked_start={clicked} text={t}");
                    break;
                }
            }
        }
    }
    if input_ready.is_none() {
        println!("STEP fail input_not_ready");
        let _ = page
            .save_screenshot(ScreenshotParams::default(), "/tmp/natural-flow.png")
            .await;
        return Ok(());
    }

    // —— 输入并回车 ——
    let focused: Option<String> = page
        .evaluate(
            r#"(() => {
                const el = document.querySelector('textarea, [contenteditable="true"], [role="textbox"], [data-slate-editor="true"], .cm-content');
                if (!el) return null;
                el.focus();
                return el.tagName;
            })()"#,
        )
        .await
        .ok()
        .and_then(|h| h.into_value::<String>().ok());
    println!("STEP focus={focused:?}");
    page.execute(InsertTextParams::new(MSG)).await?;
    let down = DispatchKeyEventParams {
        r#type: DispatchKeyEventType::KeyDown,
        modifiers: None,
        timestamp: None,
        text: Some("\r".into()),
        unmodified_text: Some("\r".into()),
        key_identifier: None,
        code: Some("Enter".into()),
        key: Some("Enter".into()),
        windows_virtual_key_code: Some(13),
        native_virtual_key_code: Some(13),
        auto_repeat: None,
        is_keypad: None,
        is_system_key: None,
        location: None,
        commands: None,
    };
    let up = DispatchKeyEventParams {
        r#type: DispatchKeyEventType::KeyUp,
        text: None,
        unmodified_text: None,
        ..down.clone()
    };
    page.execute(down).await?;
    page.execute(up).await?;
    println!("STEP sent");

    // —— 收流：点掉隐私弹层，直到 chat 响应到达或超时 ——
    // （HAR 显示 onboarding_finish 后 app 自动发送首条 prompt，弹层点击可能触发自动发送）
    let wait_deadline = Instant::now() + Duration::from_secs(90);
    let mut chat_resp_seen: Option<i64> = None;
    let mut chat_req_seen = false;
    let mut consent_clicked = false;
    let mut last_enter = Instant::now();
    let mut enters_sent = 0;
    while Instant::now() < wait_deadline {
        // 隐私弹层清障：点「继续/同意/accept」
        if let Some(t) = page
            .evaluate(
                r#"(() => {
                    const b = Array.from(document.querySelectorAll('button')).find(x => /^(继续|continue|同意|accept|agree)/i.test((x.innerText||'').trim()));
                    if (!b) return null;
                    b.click();
                    return (b.innerText||'').trim();
                })()"#,
            )
            .await
            .ok()
            .and_then(|h| h.into_value::<String>().ok())
        {
            println!("STEP consent_clicked={t}");
            consent_clicked = true;
            last_enter = Instant::now();
        }
        let item = tokio::time::timeout(Duration::from_millis(400), rx.recv()).await;
        if let Ok(Some(v)) = item {
            let kind = v["kind"].as_str().unwrap_or("");
            println!("WIRE {}", serde_json::to_string(&v)?);
            if kind == "req" && v["url"].as_str().unwrap_or("").contains(NEEDLE_CHAT) {
                chat_req_seen = true;
            }
            if kind == "resp" && v["url"].as_str().unwrap_or("").contains(NEEDLE_CHAT) {
                chat_resp_seen = v["status"].as_i64();
                break;
            }
        }
        // 同意后仍未发出 → 补按回车（限 3 次，4s 节流）
        if consent_clicked
            && !chat_req_seen
            && enters_sent < 3
            && last_enter.elapsed() > Duration::from_secs(4)
        {
            press_enter(&page).await?;
            enters_sent += 1;
            last_enter = Instant::now();
            println!("STEP re_enter={enters_sent}");
        }
        // 状态行：页面文字
        if let Some(txt) = page
            .evaluate("((document.body.innerText||'').replace(/\\s+/g,' ').slice(0,160))")
            .await
            .ok()
            .and_then(|h| h.into_value::<String>().ok())
        {
            println!("PAGE {txt}");
        }
    }

    // 剩余事件扫尾（把 status/其他响应也打出来）
    while let Ok(Some(v)) = tokio::time::timeout(Duration::from_millis(800), rx.recv()).await {
        println!("WIRE {}", serde_json::to_string(&v)?);
    }

    let ua: String = page
        .evaluate("navigator.userAgent")
        .await?
        .into_value::<String>()?;
    let wd: bool = page
        .evaluate("!!navigator.webdriver")
        .await?
        .into_value::<bool>()?;
    println!("META ua={ua} webdriver={wd}");
    let _ = page
        .save_screenshot(ScreenshotParams::default(), "/tmp/natural-flow.png")
        .await;
    println!(
        "RESULT chat_status={}",
        chat_resp_seen
            .map(|s| s.to_string())
            .unwrap_or_else(|| "none".into())
    );
    browser.close().await?;
    Ok(())
}

/// 引导按钮启发式（先用文本判断，点击由另一段 JS 完成）。
fn regexish_start(t: &str) -> bool {
    let lower = t.to_ascii_lowercase();
    [
        "start", "开始", "继续", "continue", "同意", "accept", "免费", "new chat",
    ]
    .iter()
    .any(|k| lower.contains(k))
}

/// 模拟真人回车（CDP keyDown + keyUp）。
async fn press_enter(page: &chromiumoxide::Page) -> Result<(), Box<dyn std::error::Error>> {
    let down = DispatchKeyEventParams {
        r#type: DispatchKeyEventType::KeyDown,
        modifiers: None,
        timestamp: None,
        text: Some("\r".into()),
        unmodified_text: Some("\r".into()),
        key_identifier: None,
        code: Some("Enter".into()),
        key: Some("Enter".into()),
        windows_virtual_key_code: Some(13),
        native_virtual_key_code: Some(13),
        auto_repeat: None,
        is_keypad: None,
        is_system_key: None,
        location: None,
        commands: None,
    };
    let up = DispatchKeyEventParams {
        r#type: DispatchKeyEventType::KeyUp,
        text: None,
        unmodified_text: None,
        ..down.clone()
    };
    page.execute(down).await?;
    page.execute(up).await?;
    Ok(())
}
