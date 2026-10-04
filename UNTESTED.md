# UNTESTED — 未实测项清单

本文件如实列出**尚未实测**的验证边界。以下均为「未测试」，不是已知缺陷；
每项区分「已测到哪一步 / 未测什么 / 原因」，避免把验证边界误读为质量结论。
证据文件位于构建工作区（`verify-gates-attempt2.log`、`feature-matrix.log`、`smoke/*.log`、`smoke/*.png`），未纳入本仓库。

## 1. browser 模式的成功响应路径

- **已测**：
  - `--features browser` 的编译、`clippy -D warnings`、`cargo test`（含动态头回归测试）；
  - **本地零上游端到端**：指向本地 mock（首页含 `data-version-tag`/`entry.duckai.*` 属性、
    `status` 返回可解挑战 fixture），browser 模式完成「页内取挑战 → 本地 rquickjs PoW →
    注入动态头 → 页内 fetch SSE → 回放」全链路，`/v1/chat/completions` 返回 200 + PONG；
  - **wire 头与真实抓包对照**：`accept: text/event-stream`、`priority: u=1, i`、
    `x-vqd-hash-1`（PoW 产物 `client_hashes[0]` 与真实抓包同值）、`x-fe-version`、
    `x-fe-signals`、`x-ddg-journey-id` 程序化 diff 全部在场（唯一差异为 sec-ch-ua
    品牌串：抓包用 Chromium、测试驱动真 Chrome，均属合法真实浏览器值）；
  - 418→`ERR_BN_LIMIT` 分类路径、`DUCKAI_CHROME_PATH` 指定真 Chrome 151 的运行时行为。
- **未测**：经 browser 上游对**真实 duck.ai** 拿到 200 成功响应（仅差一次已知良好出口的单发）。
- **原因**：实现修复后的真实单发落在新出口（东京机房 IP，AS49304 SAKURA）上仍 418——
  `status` 端点同出口 200、wire 头已与真实抓包逐项对照一致，归因指向出口 IP 信誉
  而非实现；需切回曾 200 的直连住宅线路补一次单发确认。
- **相关已知观察**：chromiumoxide 0.9.1 与 Chrome 151 存在 CDP 协议漂移 WARN
  （`WS Invalid message: data did not match any variant`），不影响启动与请求链路。

## 2. 真实上游的端到端覆盖不完整（http 模式）

- **已测**：t2 阶段纯 HTTP 路径真实调通 duck.ai，`/v1/chat/completions` 返回真实 `chatcmpl-` id（单次成功）；
  三协议契约由 `tests/`（fixtures + wiremock + MockUpstream）全量覆盖。
- **未测**：`/v1/messages`、`/v1/responses` 对**真实上游**的成功端到端；
  真实上游的长时 SSE 流式流量（多 chunk）。
- **原因**：同上，出口封禁后 t3 无法复现真实成功流量；契约测试刻意不访问真实上游（见 README「构建与测试」）。

## 3. 模型目录 1800s 后台热刷新

- **已测**：快照 fallback 路径有单测覆盖。
- **未测**：30 分钟周期的后台刷新任务本身——含到点触发、上游不可用时的重试、
  刷新失败后是否维持旧快照。
- **原因**：属长周期运行时行为，单测走本地快照 fallback，未做 30 分钟等待式测试。

## 4. 代理池在真实多出口环境下的行为

- **已测**：出口池接线有单测断言（原项目 P0-3「代理配置从未生效」的修复证明）、
  封禁冷却状态机（418→Banned 换端 / 429→退避 / HalfOpen 半开探测）有单测。
- **未测**：真实多代理环境下的粘性会话、健康分升降、轮换与真连接性探测。
- **原因**：验证环境只有单出口直连，无多代理拓扑可用。

## 5. 未纳入的已知非阻塞观察（非未测项，仅备忘）

- `/v1/responses` SSE 以 `response.completed` 收尾、不发 `data: [DONE]`——
  与 `ARCHITECTURE.md` 契约一致，属设计而非缺陷。
- 截图中标题分隔符 `·` 呈缺字形方框——截图容器字体环境问题，HTML 文本正常。
