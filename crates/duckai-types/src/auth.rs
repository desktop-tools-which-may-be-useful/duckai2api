//! 鉴权抽象：API 层与 WebUI 层只依赖本模块的 trait，
//! 具体落库实现（sqlite / argon2）位于 `duckai-store`，由装配层（`duckai-server`）注入。
//!
//! 依赖方向：`duckai-types` 是零依赖叶子，这里只声明契约，不得反向依赖任何 crate。

/// `/v1` 的 Bearer key 校验（实现：静态 env 默认 key / 库内多把密钥）。
pub trait ApiKeyAuth: Send + Sync {
    /// 是否存在可用密钥；`false` = 关闭鉴权（仅回环绑定允许，装配层 fail-fast 强制）。
    fn enabled(&self) -> bool;
    /// 校验客户端提交的 key（实现方负责常数时间比较/哈希查询）。
    fn validate(&self, presented: &str) -> bool;
}

/// WebUI 管理口令（实现：库内 argon2id 自定义口令，缺省回落到 env 默认口令）。
///
/// 权限语义：`available() == false` → WebUI 只读（回环可见，写操作 403）。
pub trait AdminPassword: Send + Sync {
    /// 是否配置了任何口令（库内自定义或 env 默认）。
    fn available(&self) -> bool;
    /// 库内是否已有自定义口令（true = env 默认口令已被遮蔽，救援需删库/删行）。
    fn custom_set(&self) -> bool;
    /// 校验口令（库内存在自定义口令时用 argon2id 验证，否则常数时间比较默认口令）。
    fn verify(&self, given: &str) -> bool;
    /// 写入库内自定义口令（覆盖默认）。
    fn set_custom(&self, given: &str) -> Result<(), String>;
}
