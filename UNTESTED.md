# UNTESTED — 未实测项清单

本文件如实列出**尚未实测**的验证边界。以下均为「未测试」，不是已知缺陷；
每项区分「已测到哪一步 / 未测什么 / 原因」，避免把验证边界误读为质量结论。
证据文件位于构建工作区（`verify-gates-attempt2.log`、`feature-matrix.log`、`smoke/*.log`、`smoke/*.png`），未纳入本仓库。

## 1. browser 模式的成功响应路径

- **已测**：`--features browser` 的编译、`clippy -D warnings`、`cargo test --no-run`；
  release+browser 构建启动后 `/health` 返回 `mode=browser`，chromiumoxide 成功建立 CDP WebSocket（t3 实测）。
- **未测**：经 browser 上游拿到成功响应（HTTP 200 / SSE 数据流）。
- **原因**：验证期间本机出口被上游封禁（`ERR_BN_LIMIT`，503 + `retry-after: 300`），成功路径无法触发。
- **连带未测**：`DUCKAI_CHROME_PATH` 指定自定义 Chrome 的运行时行为。
- **相关已知观察**：chromiumoxide 0.9.1 与本机 Chrome 151/152 存在 CDP 协议漂移 WARN
  （`WS Invalid message: data did not match any variant`），不影响启动与请求链路，
  但在成功路径未实测的前提下，browser 模式对新版 Chrome 的兼容性仍待确认。

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
