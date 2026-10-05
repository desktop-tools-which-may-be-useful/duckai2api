# duckai2api-rust

把 [DuckAI2API](https://github.com/eziodeng/DuckAI2API)（Python / FastAPI + Playwright）用 Rust 重写：
把 Duck.ai 兼容成三套 OpenAI / Anthropic / Responses 协议端点，协议层、API 层、UI 层严格分层，
默认**纯 HTTP 无浏览器**接入，浏览器模式按需编译。

架构设计与原项目缺陷清单见 [ARCHITECTURE.md](./ARCHITECTURE.md)（不移植其中 P0×4 / P1×7 缺陷）。

## 分层

```
duckai-types      共享类型（Turn / 事件 / 模型目录 / AdminControl 快照）   零网络零 web 依赖
duckai-protocol   协议层：事件编解码、三协议映射、文本组装                 无 axum
duckai-upstream   接入层：UpstreamClient trait + 双模式实现               无 axum
    http    （默认主路径）reqwest + VQD/挑战令牌维护 + 封禁冷却状态机 + 出口池/代理
    browser （可选 feature）Playwright 驱动
duckai-api        API 层：axum 路由、Bearer 鉴权、并发闸门 429、SSE 流式、日志环
duckai-webui      UI 层：静态控制台 + 管理 API（只依赖 duckai-types 的 AdminControl）
duckai-server     装配：.env/环境变量 → 工厂 → 路由拼装 → 监听退出
```

同一接入契约：两个适配器实现同一个 `UpstreamClient` trait，
`DUCKAI_UPSTREAM=auto|http|browser` 只是选择，不影响 API/WebUI 层。

## 快速开始

```bash
cp .env.example .env        # env 只保留引导值；密钥/口令/出口配置入 sqlite
cargo run --release         # 默认 http://127.0.0.1:8080（自动创建 data/duckai.db）
```

- 对外 API：`POST /v1/chat/completions`、`POST /v1/messages`、`POST /v1/responses`
  （SSE `stream:true` 与非流式均支持；`GET /v1/models`；设了 `DUCKAI_DEFAULT_API_KEY`
  则需 `Authorization: Bearer …`）
- 免鉴权健康检查：`GET /health`
- 控制台：浏览器打开 `http://127.0.0.1:8080/`（设置管理口令后登录获得写权限；
  不设则回环只读、写操作 403）

## 配置存储（env 引导 + sqlite 生效值）

原则：**env 只做引导，不落敏感值；生效配置与密钥入 sqlite**（rusqlite bundled，
`DUCKAI_DB_PATH` 默认 `data/duckai.db`，权限 0600，测试用 `:memory:`）。

| 存储 | 内容 | 引导规则 |
| --- | --- | --- |
| `settings` | base / vqd_override / new_chat / default_model / max_concurrency | 表空时从 env 播种一次，此后 **DB 覆盖 env** |
| `api_keys` | SHA-256 摘要 + 前 12 位前缀 + 标签 + 吊销位（**不存明文**） | 表空时导入一次 env 默认 key；明文仅创建时回显一次 |
| `passwords` | 管理口令 argon2id PHC（`set_custom`，最短 8 位） | 库内口令优先；env 默认口令为常数时间校验回落 |
| `egress` | 每出口一行：url（NULL=直连）/ 启用 / 冷却开关 / 首封·上限·429 起始秒数 / 排序 | 表空时按 `DUCKAI_PROXIES` 播种（`direct` 关键字 → 直连行），**重启后出口表是池的唯一来源** |

数据库不存的东西（如实说明）：运行期封禁状态（重启清零，管理面手动封禁兜底）、
Bearer 会话、请求日志。

## 配置（`.env.example` 全键消费对照）

| 键 | 默认 | 消费点 |
| --- | --- | --- |
| `DUCKAI_BASE` | `https://duck.ai` | 上游工厂 → HTTP 适配器所有端点拼接（空表时播种进 settings） |
| `DUCKAI_UPSTREAM` | `auto` | `UpstreamMode::parse` 选适配器（auto=有 http 用 http） |
| `DUCKAI_VQD_OVERRIDE` | 空 | 挑战求解退化链第 2 级注入（10 分钟 TTL；空表播种进 settings） |
| `DUCKAI_MODEL` | `gpt-5.6-luna` | `ApiState` 默认模型（管理面可热改，写锁实时生效） |
| `DUCKAI_NEW_CHAT` | `false` | 会话粘性开关（true=每次新会话），播种进 settings |
| `DUCKAI_PROXIES` / `DUCKAI_PROXY` | 空 | 合并去重校验；`direct` 关键字=显式直连；仅空表播种，之后以 egress 表为准 |
| `DUCKAI_DEFAULT_API_KEY` | 空 | Bearer 鉴权引导值（仅空表导入一次）；空 + 非回环 → **启动即拒绝**。遗留 `DUCKAI_API_KEY` 仍可作别名 |
| `DUCKAI_DEFAULT_ADMIN_PASSWORD` | 空 | WebUI 登录的 env 默认口令（库内口令优先）。遗留 `DUCKAI_ADMIN_PASSWORD` 仍可作别名；空=回环只读 |
| `DUCKAI_DB_PATH` | `data/duckai.db` | sqlite 路径；`:memory:` 仅供测试 |
| `PORT` / `DUCKAI_BIND` | `8080` / `127.0.0.1` | 监听地址；非回环必须配 API key |
| `DUCKAI_MAX_CONCURRENCY` | `8` | 并发闸门：超限 **429 + retry-after**（可运行时调，立即生效） |
| `DUCKAI_CHROME_PATH` | 空 | browser 模式的浏览器路径 |
| `RUST_LOG` | `info` | `tracing_subscriber::EnvFilter` |

## 构建与测试

```bash
cargo build --release
cargo test --workspace
cargo test --workspace --features browser --no-run   # 浏览器模式可编译
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
```

`cargo run` 之外的验证由 `tests/`（协议契约、API 契约、wiremock 集成）覆盖，
全程不访问真实上游网络。

**未实测项（验证边界）如实列在 [UNTESTED.md](UNTESTED.md)**，含 browser 模式成功路径、
真实上游端到端覆盖、模型目录热刷新、代理池真实多出口四类。

## 静态 WebUI（§8 方案的覆盖实现）

ARCHITECTURE.md §8 原定 vanilla TypeScript + `tsc --noEmit`。**实现按产品要求覆盖为
两份纯静态资源 + CDN，无任何构建链**（不修改 ARCHITECTURE.md，以本节为准）：

- `crates/duckai-webui/ui/index.html` + `crates/duckai-webui/ui/app.js`，经 `rust-embed` 编入二进制；
- 仅 `axios`、`highlight.js`、`tailwindcss` 三个 CDN 引用，零 npm/打包工具；
- 中文界面七个页面：状态总览、模型列表、出口池（**每出口独立策略编辑 + 启用开关 +
  直连开关 + 手动封禁/解封**）、**API 密钥管理（前缀回显、一次性明文、吊销）**、日志、
  设置（运行参数 / 代理增删 / **存储设置 base·VQD·新会话** / **改管理口令**）、
  三协议请求示例与在线试发（可选模型）。

`webui` feature 关闭时静态路由与管理 API 全部不注册（`cargo build --no-default-features --features http`）。

## 许可与署名

MIT。提交署名：`From: anonymous <anonymous@users.noreply.github.com>`。
