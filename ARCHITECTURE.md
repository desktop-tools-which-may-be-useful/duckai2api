# ARCHITECTURE.md — duckai2api-rs

DuckAI2API（Python / FastAPI + Playwright）→ Rust 重写架构设计文档。
本文件是协议层 / API 层 / UI 层实现的唯一权威依据；后续 crate、trait、端点、测试均以本文为准。

- **结论先行**：纯 HTTP 接入**可行**（多个生产级参考项目端到端验证），因此 **默认零浏览器依赖**；浏览器驱动作为 `browser` cargo feature 的可选适配器。二者共享同一个 `UpstreamClient` trait 与全部协议层构造/解析代码。
- **技术栈**：tokio + axum + reqwest（默认），JS 挑战求解用 rquickjs，静态资源用 rust-embed。

---

## 1. 原项目分析

### 1.1 现状分层职责

| 原文件 | 现状职责 | 重写后归属 |
|---|---|---|
| `duckai.py`（346 行） | 协议层：Playwright 驱动真实 Chrome 操作 duck.ai UI，拦截 `/duckchat/v1/chat` SSE 响应；模型目录快照；代理轮换与 `banned` 标记 | 协议层（改为纯 HTTP 构造 + 可选浏览器适配器） |
| `main.py`（620 行） | API 层：三协议路由（OpenAI chat / Anthropic messages / OpenAI responses）、鉴权、会话复用、SSE 转换、relay 侧工具意图路由入口 | API 层（axum router + 转换器） |
| `tools.py`（96 行） | 工具协议辅助：`<tool_call>` 信封渲染/解析、文本与工具调用切分 | 协议层共享的 tool envelope 模块 |
| `toolrouter.py`（213 行） | relay 侧工具意图路由：正则从用户话术合成 `tool_use`（Duck.ai 无原生函数工具） | API 层 tool-router 模块（**重写正则，见 P0-2**） |
| （无） | UI 层缺失：原项目只有 `/health`，README 手工配置 | UI 层（静态 WebUI + 管理 API） |

原项目对外端点：`POST /v1/chat/completions`、`POST /v1/messages`、`POST /v1/responses`、`GET /v1/models`、`GET /health`（main.py:216、496、321、481、614）；Bearer 鉴权仅在 `DUCKAI_API_KEY` 非空时启用（main.py:168-169）；监听 `0.0.0.0:8080`（main.py:620）。

### 1.2 缺陷清单（P0 = 阻断性，重写必须修正；P1 = 必修，不移植）

以下行号均已在源码中逐一复核（grep + 实测复现）。

#### P0

1. **`/v1/responses` 每次请求必然 500** —— `_responses_input_text` **全仓库无 `def` 头**（`grep "def _responses_input_text"` 零命中）。函数体 main.py:293-319 坐落在 `chat_completions` 内 `return` 之后、又顶格注释 main.py:291-292 之下，成为不可达死代码（Python 注释不影响缩进，故 `py_compile` 通过）；调用点 main.py:326 运行时 `NameError` → 500。**重写**：`/v1/responses` 输入扁平化器必须有真实实现 + 单测覆盖（字符串 / message 列表 / content blocks / tool 四种形态）。
2. **tool-router 正则贪婪截断 + 空参数（实测复现）** —— `toolrouter.py:62`（Write）、`toolrouter.py:67`（Edit）等模式中前缀 `[^`"']*` 贪婪吞噬导致文件名截断：
   - `"Write the file main.py with content foo"` → `file_path='n.py', content=''`（丢 `mai`、丢全部内容）；
   - `"Edit src/lib.rs swapping alpha"` → `file_path='b.rs'`（丢 `src/li`）；
   - `"Write 'hi' to notes.md"` → `route_intent` 返回 `None`（引号导致漏配，落回普通聊天）；
   - 注释宣称支持 `"create file path with ..."`（toolrouter.py:61）但正则只匹配 `\bwrite\b`，实测 `"create file /tmp/x.txt with hello"` → `None`；
   - `Edit` 恒产出 `old_string=''` / `new_string=''`（toolrouter.py:69），客户端拿到的是**空参数工具调用**；
   - Bash 兜底正则 `{2,300}`（toolrouter.py:79）**静默截断**超长命令；`_read_args`（toolrouter.py:54）读不到无扩展名/相对路径文件（`"read README"` → `None`）。
   **重写**：以 golden-case 单测驱动重写（锚定提取、非贪婪分组、引号容错、明确的参数完整性校验——**参数不完整宁可不路由**，也不能发出空参数 tool_use）。
3. **`DUCKAI_PROXY(S)` 代理池完全未接线** —— main.py:49-50 读取并切分 → main.py:164 传入 `DuckAISession(proxies=...)` → duckai.py:293-305 存储并轮换索引 → duckai.py:143/145 `_BrowserSession` 收下 `proxy` 后，`self.proxy` 全文件**仅 1 次出现（即赋值本身）**；`launch` 字典 duckai.py:158-162 与 `new_context` duckai.py:163 均未使用它。**换代理对被 418 的 IP 毫无作用**——README 宣称的"代理轮换"是死配置。**重写**：代理池必须在传输层真正生效，并以"418 后经代理重试成功"为集成断言。
4. **默认无鉴权 + 全接口暴露** —— `require_key` 在 `API_KEY` 为空时直接放行（main.py:168-169），而默认绑定 `0.0.0.0`（main.py:620）：开箱即向局域网/公网提供开放中继（可被第三方消耗配额、探测上游行为）。**重写**：默认 `127.0.0.1`，且"空 key + 非回环绑定"启动时拒绝（fail-fast）或强制显式 `--i-understand` 级配置。

#### P1（必修、不移植）

5. **假流式**：`send_stream` docstring 自述"UI 不可流式"，整体 `yield text` 一次性返回（duckai.py:338-342），而 README 宣称三协议"Real streaming sourced from Duck.ai's SSE"。重写按上游真实 SSE 逐 chunk 转发。
6. **封禁状态永不恢复**：`banned=True` 置位（duckai.py:334）后只有进程重启能清除；`_sessions` 从不关闭（Playwright 上下文泄漏）；418 无 Retry-After（duckai.py:267）。重写为显式冷却状态机（§6）。
7. **死配置**：`DUCKAI_NEW_CHAT` 读取后从未使用（main.py:52 仅赋值）；`DUCKAI_MAX_CONCURRENCY` 只存在于 README/example.env，代码中无任何并发闸门 → 上游无背压保护。
8. **模型目录硬编码快照**：`MODEL_LABELS` 9 模型快照（duckai.py:51 起，2026-08-27 拍板），未知模型原样透传上游导致上游 4xx。重写为"启动拉取 + 缓存 + 快照兜底"，未知模型本地 404。
9. **平台假设**：`CHROME_PATH` 默认 Windows 路径（duckai.py:42）；`channel="chrome"` 要求本机装 Chrome——浏览器特性必须可选（§5），纯 HTTP 默认路径不得依赖任何浏览器。
10. **README 漂移**：`cp .env.example .env`（实际文件叫 `example.env`）；文档端点 `/duckchat/api/*` + `X-Vqd-4` 与真实实现 `/duckchat/v1/*` + `x-vqd-hash-1` 不符；"代理轮换"承诺因 P0-3 不成立。重写文档随代码同 PR 更新。
11. **工具信封半死**：`render_tools_prompt` 被 import（main.py:38）但全仓库**零调用**（仅定义 tools.py:34）；`parse_tool_call` 同样只 import 未调用。重写后要么完整接入（信封注入 + 解析），要么删除，不留 import 僵尸。

---

### 1.3 保留的正确设计（不要在重写中丢掉）

- 上游错误分类与映射（`DuckAIRateLimit` / `DuckAIError` → 三协议各自的错误帧）。
- relay 侧工具路由的**安全模型**：只合成 `tool_use`、绝不在服务端执行工具（toolrouter.py:14-15 的设计动机依旧成立，修正的只是解析质量）。
- `flatten_conversation` 的角色标签扁平化策略（多轮上下文保序）。
- 拦截上游 SSE 响应而非抓 DOM 的思路（duckai.py docstring 第 3 条）——纯 HTTP 模式天然满足。

---

## 2. 参考项目调研：纯 HTTP 接入方案

### 2.1 调研矩阵

| 仓库 | 语言/栈 | 关键证据 | 对本项目的启示 |
|---|---|---|---|
| [aurora-develop/Duck2api](https://github.com/aurora-develop/Duck2api)（Go，~814★） | Go + goja | `internal/duckgo/vqd.go`（`GET /duckchat/v1/status` + `x-vqd-accept:1` 取 `x-vqd-hash-1` 挑战、goja 求解、`vqd_test.go` 内置真实挑战 fixture 通过）、`request.go`（`InitXVQD`、`x-fe-version` 30 分钟缓存、`x-fe-signals` 构造、`POSTconversation` 418/429/400/ERR_CHALLENGE 三连重试、`bogdanfinn/tls_client` 指纹化 TLS） | **纯 HTTP 全流程模板**；挑战 fixture 化测试方法直接抄 |
| [amirkabiri/duckai](https://github.com/amirkabiri/duckai)（TS，~134★） | TS + JSDOM | `getEncodedVqdHash`：`x-vqd-hash-1` → base64 JS → JSDOM 沙箱 iframe 求值 → 各 `client_hashes` 做 SHA-256 → base64；429 按 `retry-after` 重试，~20 req/min 限额 | 挑战求值需要**DOM 静态**（document/window/iframe 桩）的实现细节 |
| [0x676e67/duckai](https://github.com/0x676e67/duckai)（Rust，~42★） | **Rust：axum + rquest + hickory-resolver + moka + eventsource-stream** | 与本项目技术栈同构的 Rust 先例；`rquest` `.impersonate(random_impersonate())` 说明 **JA3 指纹化是 Rust 侧公认必要项** | Rust 栈可行性背书；TLS 指纹能力选项 |
| [Afqoro/duckai-bridge](https://github.com/Afqoro/duckai-bridge)（Go） | Go + goja | 完整 `ChatRequest`/`durableStream`（RSA-OAEP-256 JWK）/SSE 事件结构（`assistant`/`tool-invocation`/`source`/`[PING]`/`[DONE]`）+ `parseSSE`；418/429 换新 hash 重试一次；`internal/vqdcapture` 浏览器捕获兜底（自述"mock 里本地求解通过、生产要真实流量"） | **请求体与 SSE 解析的事实标准**；挑战求解双通道设计（本地求解 + 浏览器捕获兜底） |
| [OmarElsiry/duck-proxy](https://github.com/OmarElsiry/duck-proxy) | Python 参考 + **`duck-proxy-rs`（axum + reqwest + deno_core V8 + rsa + wiremock）** | Rust 侧用 V8 跑挑战 JS，`src/v8/stubs.js` 提供 DOM/crypto 桩，`src/duck/{client,stream,payload}.rs` + `src/crypto/jwk.rs`；40 个场景测试 | 挑战求解**可替换引擎**的先例（QuickJS 轻量 vs V8 高保真）；`durableStream` JWK 处理 |
| [nekohy/duck2api](https://github.com/nekohy/duck2api)（TS） | TS | `vm.ts` 沙箱模式被 duckai-bridge 引为出处 | 参考即可 |
| [mumu-lhl/duckduckgo-ai-chat-service](https://github.com/mumu-lhl/duckduckgo-ai-chat-service)（Go，~84★）、[mrgick/duck_chat](https://github.com/mrgick/duck_chat)（~111★） | Go/Rust | DuckDuckGo AI Chat 的另一路 VQD 端点形态 | 旁证生态，非主路径 |
| [HEXUXIU/M365-Copilot2API](https://github.com/HEXUXIU/M365-Copilot2API)（Go，~529★） | Go | `internal/{auth,chathub,config,mcp,outbound,web}` 分层、`connpool/proxy/health` 代理池与健康检查、HAR 挖掘文档 | **网关分层与代理池/健康检查形态**参考 |
| [jasonxu114514/opencode2api](https://github.com/jasonxu114514/opencode2api)（Go，~479★） | Go | `internal/{gateway,config,httpx,identity,admin}`、`pool/refresh/upstream/health` | **upstream 池 refresh/health 模式**参考 |
| [hirotomasato/duckapi](https://github.com/hirotomasato/duckapi) | Python + Playwright | 浏览器驱动路线的另一实例（与原项目同源思路），README 详述 418 永久性与 IP 风险 | 仅作浏览器模式设计与风险表述参考 |

### 2.2 纯 HTTP 协议重构（可实施全流程）

1. **取挑战**：`GET https://duck.ai/duckchat/v1/status`，请求头 `x-vqd-accept: 1` → 响应头 **`x-vqd-hash-1`** = base64 编码的挑战 JavaScript 源码。
2. **求解 PoW**：解码 JS，在 JS 引擎中执行，注入桩环境（`atob/btoa`、`crypto.subtle.digest`=SHA-256、`window.parent`、iframe `srcdoc`、`document.querySelector`、`__DDG_BE_VERSION__`、`__DDG_FE_CHAT_HASH__`）。得到 `{server_hashes, client_hashes, signals, meta}`；对每个 `client_hashes` 做 SHA-256 → base64；`meta` 注入 `{origin:"https://duck.ai", stack:"Error\n at l (entry…js…)", duration_ms}`；`JSON.stringify` → base64 → 作为请求头 `x-vqd-hash-1`。失败路径：`base64(decoded + "::" + message + "::" + stack + "::" + origin)`。
3. **`x-fe-version`**：`GET https://duck.ai/` 抓 `data-version-tag` + `data-version-sha` → `"<tag>-<sha>"`，缓存约 30 分钟（aurora `InitFEVersion`）。
4. **`x-fe-signals`**：base64(JSON 事件日志)：`onboarding_impression` → `action`(`trusted:true`) → `onboarding_finish` → `startNewChat_free`，各事件间隔随机化（aurora `CreateFESignals`）。
5. **`x-ddg-journey-id`**：随机 16 字节 hex。
6. **发问**：`POST https://duck.ai/duckchat/v1/chat`，头：`accept: text/event-stream`、`content-type: application/json`、`origin/referer: https://duck.ai`、`sec-ch-ua` / `sec-fetch-*`、真实 Chrome UA、`x-fe-signals`、`x-fe-version`、`x-ddg-journey-id`、`x-vqd-hash-1`。
   体：`{model, messages[{role,content}], canUseTools, reasoningEffort, metadata{toolChoice{WebSearch,GenerateImage,NewsSearch,VideosSearch,LocalSearch,WeatherForecast,RelatedSearchTerms,AsksClarifyingQuestions}}, durableStream{messageId, conversationId, publicKey(JWK, RSA-OAEP-256；私钥仅本地，实践中上游回明文 JSON)}}`。**注意**：duck.ai 只支持内置工具（toolChoice），**不支持用户自定义 function tools**——这正是原项目 relay 侧工具路由存在的原因，重写必须保留该桥（并修好 P0-2）。输入超长按上游限制截断。
7. **解析 SSE**：`data: {"role":"assistant","message":"…"}`（逐段）、`data: {"role":"tool-invocation","state":"call"|"result","toolName","toolArguments"}`、`data: {"role":"source",…}`、`data: [CHAT_TITLE:…]`、`data: [PING]`、`data: [DONE]`。
8. **错误与重试**：418 / 429 / 400 / `ERR_CHALLENGE` → 作废当前 VQD → 重新 `InitXVQD` → 重试（aurora ≤3 次；duckai-bridge ≤1 次 + 新 hash）。429 优先遵守 `retry-after`。

### 2.3 可行性与风险评估

- **可行性：高。** 至少 4 个不同语言的项目（aurora/Go 生产级 + TS + Rust + Go bridge）以纯 HTTP 完整跑通同一套协议，且 aurora 有真实挑战 fixture 的通过测试、duckai-bridge 有对 `gpt-5.6-luna` 的端到端记录。原项目"必须浏览器"的结论只对**它自己的两条死路**成立：裸 `fetch('/duckchat/v1/chat')` 缺挑战头（ERR_CHALLENGE），Playwright 内置 Chromium 指纹被 418——**不构成纯 HTTP 不可行的证据**；参考项目恰好证明了补齐挑战/签名头即可。
- **风险与对策**：
  - **IP/指纹层面 418 `ERR_BN_LIMIT`**（无 Retry-After、持久化；上游匿名、按 IP+指纹判定，不存在"账号被封"）：§6 代理池 + 冷却状态机 + 并发闸门；住宅/SOCKS5 优先；传输层指纹尽量接近 Chrome（§5.3）。
  - **挑战 JS 变更**：`x-fe-version` 随页面版本滚动 + 挑战求解与页面实现解耦失败时进入退化通道（§5.4）。
  - **PoW 求解与真实 DOM 存在差距**（duckai-bridge 自述"mock 通过、生产需真实流量"）：求解器做成可替换组件（QuickJS 默认 / 必要时 V8），并保留"捕获的 `x-vqd-hash-1` 注入"运维后门。
  - **JA3/HTTP2 指纹**：默认 reqwest（rustls）与 Chrome 不同；以 header 序列 + ALPN 对齐为第一道缓解，`impersonate` feature 为第二道（§5.3）。

### 2.4 回退决策（明确）

> **决策：纯 HTTP 为默认且被认定可行，不回退到"浏览器必需"架构。** 浏览器仅作为 `browser` feature 的适配器保留，用于两种情形：(a) 上游挑战升级到本地 JS 引擎无法模拟、且退化通道失效；(b) 运维显式选择最高保真度。任何情形下 API 层/UI 层/协议层其余部分**不得**感知当前处于哪种模式。

---

## 3. Workspace crate 布局

```
duckai2api-rs/
├── Cargo.toml                     # [workspace]，rust-version 1.83+，edition 2024
├── ARCHITECTURE.md                # 本文档
├── crates/
│   ├── duckai-types/              # 零依赖基础：ID、错误类型、事件类型、模型描述
│   │   └── src/{lib,error,event,model}.rs
│   ├── duckai-protocol/           # ★协议层（无 I/O 派发，纯构造/解析 + 求解器）
│   │   ├── src/
│   │   │   ├── lib.rs
│   │   │   ├── chat.rs            # 请求体构造（ChatRequest/durableStream/toolChoice）
│   │   │   ├── headers.rs         # x-fe-version / x-fe-signals / x-ddg-journey-id / 通用头
│   │   │   ├── vqd.rs             # VQD 生命周期：取挑战、求解、缓存、失效、重试策略
│   │   │   ├── pow/               # 挑战求解：engine trait + rquickjs 实现 + DOM 桩
│   │   │   ├── sse.rs             # 上游 SSE 解析 → UpstreamEvent 流
│   │   │   ├── flatten/{openai,anthropic,responses}.rs   # 三协议输入扁平化（P0-1 修正位）
│   │   │   ├── tool_envelope.rs   # <tool_call> 信封渲染/解析（原 tools.py）
│   │   │   └── tool_router.rs     # 意图路由（原 toolrouter.py，重写，P0-2 修正位）
│   │   └── tests/                 # 挑战 fixture 回放、SSE golden、flatten/tool_router golden
│   ├── duckai-upstream/           # ★接入层：UpstreamClient trait 实现与选型
│   │   ├── src/
│   │   │   ├── lib.rs             # trait 定义 + UpstreamFactory（auto/http/browser）
│   │   │   ├── http.rs            # 默认适配器：reqwest + §2.2 全流程（feature "http"，默认）
│   │   │   ├── browser.rs         # 可选适配器：chromiumoxide 驱动系统 Chrome（feature "browser"）
│   │   │   ├── pool.rs            # 代理池 + 健康分 + 换端选择
│   │   │   └── cooldown.rs        # 封禁冷却状态机（§6）
│   │   └── tests/                 # wiremock 模拟 duck.ai 全流程；418→代理切换集成测试
│   ├── duckai-api/                # ★API 层：axum router、鉴权、三协议转换、背压
│   │   ├── src/{lib,router,auth,sse_out,handlers/{chat,messages,responses,models,health}}.rs
│   │   └── tests/                 # 以 MockUpstream 驱动的三协议契约测试
│   ├── duckai-webui/              # ★UI 层：静态资源（rust-embed）+ 管理 API handler
│   │   ├── src/{lib,admin_api,assets.rs}
│   │   └── ui/                    # 源码：vanilla TS + CSS（无 Node 构建；可选 tsc --noEmit 检查）
│   │       ├── index.html
│   │       └── src/{main.ts,api.ts,pages/{dashboard,models,proxies,logs,settings}.ts}
│   └── duckai-server/             # 组装二进制：配置、日志、优雅退出、bind 校验（P0-4）
│       └── src/main.rs
├── fixtures/                      # 上游响应样本（脱敏）：challenge JS、status、SSE 各事件
└── tests/                         # 跨 crate 端到端（wiremock 上游 + 真 axum server）
```

依赖方向（单向，禁止反向）：

```
duckai-server → duckai-api → duckai-upstream → duckai-protocol → duckai-types
                    └────────→ duckai-webui ──┘（webui handler 只依赖 types + upstream 的只读状态快照）
```

Cargo features：

| feature | 默认 | 作用 |
|---|---|---|
| `http` | ✅ | reqwest 纯 HTTP 适配器 |
| `browser` | ❌ | chromiumoxide 浏览器适配器（引入 CDP 依赖树） |
| `pow-v8` | ❌ | 挑战求解切换 deno_core（QuickJS 保真度不足时的升级通道） |
| `impersonate` | ❌ | 传输层 JA3/HTTP2 指纹化（`reqwest-impersonate` 一类实现；见 §5.3） |
| `webui` | ✅ | 嵌入静态资源；关闭时仅 API |

---

## 4. `UpstreamClient` trait 与 Mock

```rust
// duckai-upstream/src/lib.rs
use duckai_types::{UpstreamEvent, UpstreamError, ModelInfo, ChatTurn, ToolChoice};

/// 一次对话请求（协议无关，已由 duckai-protocol 扁平化）
#[derive(Debug, Clone)]
pub struct UpstreamRequest {
    pub model: String,
    pub turns: Vec<ChatTurn>,          // role + content（含 tool 角色结果）
    pub tool_choice: Option<ToolChoice>, // 仅 BUILT-IN 工具集
    pub reasoning_effort: Option<String>,
    pub session_hint: Option<String>,  // 同会话粘性（选代理/VQD 复用）
}

#[async_trait::async_trait]
pub trait UpstreamClient: Send + Sync {
    /// "http" | "browser"，仅供观测与 /health 展示。
    fn mode(&self) -> &'static str;

    /// 可用模型（上游拉取结果或快照兜底）。
    async fn list_models(&self) -> Result<Vec<ModelInfo>, UpstreamError>;

    /// 流式对话；返回的 Stream 按上游真实节奏 yield（修正 P1-5 假流式）。
    /// 挑战失效/418/429 的内部重试（§2.2 第 8 步）对调用方透明，
    /// 重试耗尽才向上抛分类错误。
    async fn chat(
        &self,
        req: UpstreamRequest,
    ) -> Result<BoxStream<'static, Result<UpstreamEvent, UpstreamError>>, UpstreamError>;

    /// 轻量探活（更新 x-fe-version / 代理健康），供 /health 与 WebUI 调用。
    async fn probe(&self) -> Result<(), UpstreamError>;
}
```

`UpstreamEvent`（duckai-types）：`TextDelta(String)` / `ReasoningDelta(String)` / `ToolCall{id,name,arguments}` / `ToolResult{…}` / `Source{url,title}` / `Ping` / `Title(String)` / `Done{finish_reason}`。API 层把同一条事件流翻译成 OpenAI `chat.completion.chunk`、Anthropic `content_block_*`、Responses `response.output_text.delta` 三种帧。

`UpstreamError` 分类（决定 API 层响应）：`RateLimited{retry_after}` → 429+`retry-after`；`Banned{scope:Egress}` → 503+`retry-after`（**措辞禁止出现"账号被封"**，上游匿名；只表述为 IP/指纹受限）；`ChallengeFailed` → 503（退化通道已触发）；`ModelNotFound` → 404；`Upstream(status,body)` → 502；`InvalidInput` → 400。

**MockUpstream**（`duckai-upstream/src/testkit.rs`，随 crate 导出，feature `testutil`）：

```rust
pub struct MockUpstream {
    mode: &'static str,
    models: Vec<ModelInfo>,
    script: Mutex<VecDeque<ScriptedTurn>>,  // 每次 chat() 消费一条
    seen: Mutex<Vec<UpstreamRequest>>,      // 断言入参
}
pub enum ScriptedTurn {
    Chunked(Vec<UpstreamEvent>, Duration),  // 逐帧 + 间隔，验证流式节奏
    ToolCall { name: String, args: serde_json::Value },
    Fail(UpstreamError),                    // 注入 429/418/Challenge…，验证重试与状态机
    Hang,                                   // 验证 API 层超时与背压
}
```

API 层全部契约测试仅依赖 MockUpstream（不碰网络）；`duckai-api/tests` 断言：三种协议对同一脚本的字节级输出、`Fail` 注入时的状态码/头、`Chunked` 时 SSE 逐帧到达（非一次性）。

---

## 5. 双模式接入设计（without-browser 默认 / with-browser 可选）

### 5.1 形态

- **默认（无 Playwright/CDP 依赖）**：`http` feature，纯 reqwest 走 §2.2 全流程。二进制默认特性集合即可在无浏览器环境运行。
- **可选**：`browser` feature 编译 `browser.rs`——用 **chromiumoxide（CDP，crates.io 0.9.x）驱动系统 Chrome**（`playwright-rust` 无稳定版不可选；chromiumoxide 走原项目同款"真实 Chrome + 拦截 `/duckchat/v1/chat` 响应"路径，但用 CDP `Fetch`/`Network` 拦截，不依赖 Playwright）。启动参数沿用 `--disable-blink-features=AutomationControlled`、真实 UA、剥离 `navigator.webdriver`。
- **共享点**：两种模式实现**同一个** `UpstreamClient`，复用 `duckai-protocol` 的请求体、头构造、SSE 解析、三协议扁平化、tool 路由；差异仅在"挑战如何获得"与"字节如何发出"。

### 5.2 选型

```toml
# config（环境变量/文件同构）
DUCKAI_UPSTREAM=auto|http|browser   # 默认 auto：有 http 用 http；编译含 browser 且 http 连续退化时切 browser
```

`UpstreamFactory::build(cfg, pool, vqd_store) -> Arc<dyn UpstreamClient>`；`auto` 的切换是**运行时降级**，切换事件写日志并在 `/health`、WebUI 暴露 `active_mode`。

### 5.3 TLS/指纹（http 模式）

1. 固定 header 集与顺序对齐 §2.2（含 `sec-ch-ua` 全套）、Chrome 系 UA、`accept-language`；
2. 连接复用 + keep-alive/HTTP2 设置贴近浏览器；
3. `impersonate` feature（可选编译）切换指纹化传输；未编译时接受略高的 418 概率，由 §6 代理池吸收。
   （依据：Rust 参考项目普遍采用 `rquest`/`tls_client` 级指纹化；本项目将其**做成开关而非硬依赖**，保持默认栈=reqwest。）

### 5.4 挑战求解退化链（http 模式内）

`本地求解(rquickjs, 默认)` → 失败 `N` 次 → `注入运维捕获的 x-vqd-hash-1（环境变量/文件，带 TTL）` → 仍失败 → `auto 模式下提议切 browser（编译含 browser 时）` → 否则 503 + 可操作错误信息。
求解器为 `trait PowEngine { fn solve(&self, challenge_js: &str, env: &PowEnv) -> Result<PowSolution, PowError>; }`，`rquickjs` 为默认实现，`pow-v8` feature 提供 deno_core 实现——与 duck-proxy-rs 的选择对齐但不默认背 V8 重量。

---

## 6. 封禁冷却状态机与代理池

### 6.1 单出口（egress）状态机

一个 egress = 直连或一条代理。状态与迁移：

```
            成功chat                429(retry-after)
  ┌──────────────────────┐   ┌──────────────────────────────┐
  │                      ▼   ▼                              │
Healthy ──418 ERR_BN_LIMIT──► Cooldown ──到期──► Healthy(半开)
  │                           │  │
  │                           │  └─连续失败≥K──► Banned(长冷却)
  │                           └─ 418持久型 ──► Banned         │
  └── 挑战失效(400/ERR_CHALLENGE)：不换egress，触发VQD重取+重试(≤3) ◄─┘

Banned ──冷却到期──► HalfOpen(探测1次) ──成功──► Healthy
                   └─失败──► Banned(冷却×2，上限 24h；可配置永久)
```

要点：

- **429** → 尊重 `Retry-After`，缺省指数退避（5s→10s→…，上限 10min），egress 与全局并发闸门同时收紧。
- **418 `ERR_BN_LIMIT`** → 该 egress 直接 `Banned`（持久型、无 `Retry-After`），**立即切换到下一条健康 egress 重放同一请求**（最多换端 K=2 次），这正是原项目缺失的"换代理真的生效"（P0-3 修正）。
- 挑战类错误**不消耗 egress**：只让 `vqd_store` 失效重取，避免把上游行为误判成 IP 受限。
- 所有状态迁移带时间戳与原因码，持久化于内存（重启清零可接受）并在 WebUI 实时展示。
- **全局闸门**：`max_concurrency`（默认 8，修正 P1-7）+ 每 egress 并发上限 2；获取不到令牌时在 API 层排布 429+`retry-after`，而不是把请求砸向上游。

### 6.2 代理池

- 配置：`DUCKAI_PROXIES`（逗号分隔，兼容原变量名）或 `DUCKAI_PROXY`；支持 `http/https/socks5`；为空=直连 egress。
- 选择：Healthy 集合按 `健康分`（成功+1 / 429-2 / 418-10，滑动窗口）降序 + 同分会话粘性（`session_hint` 哈希）挑选；Cooldown/Banned/HalfOpen 不参与常规分发。
- **传输层真正生效**：每条请求经 `pool.acquire()` 得到的代理直接配置到 reqwest（或 CDP 浏览器上下文）——以集成测试断言（wiremock 上游观察到出口 IP 变化可退化为"代理中间件收到请求"断言）。
- 运维后门：WebUI 可手动 Banned/解封单条代理、触发全量 probe。

---

## 7. 三协议端点清单（对外 API）

| 方法 | 路径 | 鉴权 | 流式 | 说明 |
|---|---|---|---|---|
| POST | `/v1/chat/completions` | Bearer* | SSE (`stream:true`) / JSON | OpenAI Chat；含 relay 工具路由（修复 P0-2 后） |
| GET | `/v1/models` | Bearer* | JSON | 模型列表（上游缓存 + 快照兜底，修 P1-8）；含 `owned_by`/别名 |
| POST | `/v1/messages` | Bearer* | SSE / JSON | Anthropic Messages：`message_start/content_block_start/delta/stop`、`tool_use`、`ping` |
| POST | `/v1/responses` | Bearer* | SSE / JSON | OpenAI Responses：输入扁平化（**P0-1 修正**）、`response.output_text.delta`、`response.completed` |
| GET | `/health` | 无 | JSON | `{status, upstream:{mode, vqd_valid, fe_version_age}, egress:{healthy,total,banned}, inflight}` |
| GET | `/`、`/ui/*` | WebUI 管理口令（见 §8） | HTML/静态 | 静态 WebUI |
| GET/POST | `/admin/api/*` | 同上 | JSON | WebUI 后端：代理、状态机、日志、配置视图 |

\* Bearer：`DUCKAI_API_KEY` 非空时强制；为空时仅允许回环绑定，否则启动失败（P0-4）。

上游侧（**不对外暴露**）：`GET {BASE}/duckchat/v1/status`、`POST {BASE}/duckchat/v1/chat`、`GET {BASE}/`（fe-version 抓取），`BASE` 默认 `https://duck.ai`（保留 `DUCKAI_BASE` 变量）。

统一错误帧：OpenAI 走 `{"error":{type,code,message,param}}`；Anthropic 走 `{"type":"error","error":{type,message}}`；`retry-after` 头在 429/503 一律存在。

---

## 8. 静态 WebUI 方案

- **技术**：vanilla TypeScript + 手写 CSS（无框架、无打包器），构建期用 `tsc --noEmit` 做类型检查，产物经 `rust-embed` 编入二进制；`webui` feature 关闭时全部静态路由不注册。
- **页面**：
  1. **Dashboard**：`/health` 数据实时刷新——活跃模式、VQD 剩余有效期、fe-version 年龄、在途请求、最近错误；
  2. **Models**：上游模型列表 + 别名 + 默认模型选择（写入配置端点）；
  3. **Egress/代理**：状态机每条 egress 的状态时间线、健康分、手动禁用/解封、代理增删（即 §6 的可视化与操作面）；
  4. **Logs**：内存环形缓冲（默认 500 条）的结构化请求日志——时间、协议、模型、耗时、结果码、重试/换端原因（脱敏：不打印上游响应全文与密钥）；
  5. **Settings**：只读展示有效配置 + 关键开关（默认模型、并发上限、代理列表）。
- **安全**：管理口令（`DUCKAI_ADMIN_PASSWORD`）换取会话 cookie；未配置口令时 WebUI **只读且仅回环**可访问，任何写操作端点 403；与开放中继风险（P0-4）一起在启动校验中兜底。
- **实现约束**：页面全部通过 `/admin/api/*` 与 `/health` 取数，不引入服务端模板；`duckai-webui` 只依赖 types 与只读状态快照接口，禁止反向依赖 API 层内部。

---

## 9. 测试策略

| 层 | 工具 | 内容 | 对应缺陷 |
|---|---|---|---|
| 单元（protocol） | `cargo test` + golden fixtures | ① 挑战求解：`fixtures/challenge_*.js` 回放（仿 aurora `vqd_test.go`），断言产出 hash 稳定、环境桩完备；② SSE 各事件 → `UpstreamEvent` 映射；③ 三协议 flatten 黄金用例（字符串/list/tool/content blocks）；④ **tool_router 黄金用例 ≥20 条**（含 §1.2 P0-2 全部实测反例：`main.py` 不得截成 `n.py`、引号、create-file、超长命令、无扩展名 Read；参数不完整 → 不路由）；⑤ 信封渲染/解析 round-trip | P0-1、P0-2、P1-5/11 |
| 集成（upstream） | `wiremock` 模拟 duck.ai | status→挑战→chat 全流程；断言请求头（`x-vqd-hash-1`/`x-fe-version`/`x-fe-signals`/`x-ddg-journey-id`/sec-ch-ua）与请求体形状；**418 注入 → 状态机 Banned → 切换代理 → 二次成功**（P0-3 的核心断言）；429+`retry-after` 退避；挑战失效重取（≤3 次）；fe-version 30min 缓存 | P0-3、P1-6 |
| 契约（api） | MockUpstream + axum `oneshot` | 三协议 × {流式/非流式/工具/错误注入} 的字节级输出；`/v1/responses` 回归用例（P0-1）；429/503 头；鉴权矩阵（空 key+回环=允许、空 key+非回环=启动失败，P0-4）；SSE 逐帧到达断言 | P0-1、P0-4、P1-5/7 |
| 状态机/池 | 表驱动单测 | §6 全迁移矩阵（含半开探测、退避上限、会话粘性、健康分衰减） | P1-6 |
| UI | `cargo test`（handler 层）+ 静态检查 | admin API 鉴权、只读模式 403；`tsc --noEmit`；embed 资源 404/SPA 路由 | §8 |
| 浏览器模式 | `#[ignore]` + `DUCKAI_BROWSER_E2E=1` | 仅本机、不进 CI：启动系统 Chrome、真实上游一轮对话 | §5.1 |
| 质量门 | CI 必跑 | `cargo fmt --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo nextest run --workspace`（或 `cargo test`）、`cargo test --doc` | 全部 |

夹具纪律：`fixtures/` 中的上游样本必须脱敏（去 IP/令牌/个人信息）；每个线上协议行为变化（挑战格式、事件类型）落地为新 fixture + 测试后才改实现。

本地验证命令（交付验收即跑这套）：

```bash
cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings \
  && cargo test --workspace && cargo test --workspace --features browser --no-run
```

---

## 10. 依赖基线（crates.io 已核实存在）

`tokio 1.53`、`axum 0.8`、`reqwest 0.13`、`async-trait`、`eventsource-stream 0.2`、`rquickjs 0.14`、`serde/serde_json`、`sha2`、`base64`、`rand`、`rsa`（durableStream JWK）、`dashmap 6.2`、`rust-embed 8.12`、`tracing`、`thiserror`、`wiremock 0.6.5`（dev）、`chromiumoxide 0.9`（`browser` feature 才引入）。可选：`reqwest-impersonate`（`impersonate` feature）、`deno_core`（`pow-v8` feature）。`playwright-rust`（无稳定版）与 Playwright/Python 一律不引入。

---

## 11. 里程碑（供排期，非本任务交付）

1. **M1 协议层**：vqd/pow/headers/sse/flatten/tool_router + 全部单元黄金测试（P0-1/P0-2 在此关闭）。
2. **M2 上游接入**：http 适配器 + 池/状态机 + wiremock 集成（P0-3、P1-6/7 在此关闭）。
3. **M3 API 层**：三协议 + 鉴权 + MockUpstream 契约测试（P0-4、P1-5 关闭）。
4. **M4 UI 层**：WebUI + admin API。
5. **M5（可选）**：browser 适配器、`impersonate`/`pow-v8` features。
6. **M6 发布**：README 与实现同步（P1-10）、以 `From: anonymous <anonymous@users.noreply.github.com>` 推送组织仓库。
