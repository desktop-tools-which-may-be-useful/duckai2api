use serde::{Deserialize, Serialize};

/// /health：`{status, upstream:{mode,vqd_valid,fe_version_age}, egress:{healthy,total,banned}, inflight}`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthSnapshot {
    pub status: String,
    pub upstream: UpstreamHealth,
    pub egress: EgressHealth,
    pub inflight: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpstreamHealth {
    /// "http" | "browser"
    pub mode: String,
    pub vqd_valid: bool,
    /// fe-version 距上次刷新的秒数（从未抓取为 null）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fe_version_age: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EgressHealth {
    pub healthy: usize,
    pub total: usize,
    pub banned: usize,
}

/// 单条出口的状态快照（WebUI Egress 页时间线的渲染单元）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EgressSnapshot {
    /// 出口序号。
    pub index: usize,
    /// 脱敏标签：`direct` 或 `proxy#N`（绝不携带代理凭据）。
    pub label: String,
    /// Healthy | Cooldown | Banned | HalfOpen
    pub state: String,
    /// 迁移时间戳（epoch ms）。
    pub since_ms: u64,
    /// 迁移原因码（如 `418 ERR_BN_LIMIT` / `429 retry-after=7` / `success`）。
    pub reason: String,
    /// 滑动窗口健康分。
    pub score: i64,
    pub inflight: usize,
    /// 代理 URL（脱敏，仅协议+主机；无凭据）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,
}

/// 结构化请求日志条目（环形缓冲，默认 500 条；不含上游响应全文与任何密钥）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    /// epoch ms
    pub ts_ms: u64,
    /// chat | messages | responses
    pub protocol: String,
    pub model: String,
    pub duration_ms: u64,
    /// HTTP 结果码。
    pub status: u16,
    /// 内部重试次数。
    pub retries: u32,
    /// 换端/退避原因（如 `egress#1 -> proxy#2 (418)`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub switch_reason: Option<String>,
    /// 错误分类码（成功为空）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 库内 API 密钥元数据（展示用；绝不含明文与完整哈希）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeyInfo {
    pub id: i64,
    /// 人工标签（如「网关 A」）。
    pub label: String,
    /// 展示前缀（如 `sk-Vk9…` 前 12 字符），用于对账。
    pub prefix: String,
    /// 创建时间（epoch 秒）。
    pub created_at: i64,
    /// 吊销时间（epoch 秒）；`None` = 活跃。
    pub revoked_at: Option<i64>,
}

impl ApiKeyInfo {
    pub fn active(&self) -> bool {
        self.revoked_at.is_none()
    }
}

/// 只读状态视图（Settings 页 + Dashboard 数据源）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettingsSnapshot {
    pub default_model: String,
    pub max_concurrency: usize,
    pub bind: String,
    /// 非空时 API 强制鉴权。
    pub auth_enabled: bool,
    pub admin_password_set: bool,
    pub upstream_mode: String,
    pub new_chat: bool,
    /// 脱敏代理列表。
    pub proxies: Vec<String>,
}

/// WebUI 只读数据源（实现于 duckai-server；UI 层只依赖本 trait，禁止反向依赖）。
pub trait AdminState: Send + Sync {
    fn health(&self) -> HealthSnapshot;
    fn egresses(&self) -> Vec<EgressSnapshot>;
    /// 最近 n 条（n<=环形容量）。
    fn logs(&self, lines: usize) -> Vec<LogEntry>;
    fn settings(&self) -> SettingsSnapshot;
    fn models(&self) -> Vec<crate::ModelInfo>;
}

/// 管理写操作（手动 ban/unban、代理增删、探测、配置开关）。
/// 实现方负责并发安全；鉴权与只读模式由 UI 层入口强制。
pub trait AdminControl: AdminState {
    /// 手动封禁出口（WebUI 后门）。
    fn ban_egress(&self, index: usize) -> Result<(), String>;
    /// 解封出口（P1-6：banned 不再只靠重启恢复）。
    fn unban_egress(&self, index: usize) -> Result<(), String>;
    /// 触发全量探测（async 由实现方自行 spawn；入口保持同步以适配零依赖 trait）。
    fn trigger_probe(&self);
    fn add_proxy(&self, url: String) -> Result<(), String>;
    fn remove_proxy(&self, url: String) -> Result<(), String>;
    fn set_default_model(&self, model: String) -> Result<(), String>;
    fn set_max_concurrency(&self, n: usize) -> Result<(), String>;
}
