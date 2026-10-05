//! 管理口令实现：库内 argon2id 自定义口令优先，`DUCKAI_DEFAULT_ADMIN_PASSWORD` 回落。
//!
//! 语义：
//! - 库内有行 → 只认库（env 默认被遮蔽；救援 = 删 `admin_password` 行或删库）；
//! - 库内无行 → 认 env 默认（常数时间比较）；`set_custom` 后写入库内并遮蔽默认；
//! - 两者皆无 → `available() == false`，WebUI 只读（回环）。

use std::sync::Arc;

use duckai_store::Store;
use duckai_types::AdminPassword;

/// 自定义口令最短长度（仅约束写入，env 默认口令不受限以兼容既有部署）。
const MIN_PASSWORD_LEN: usize = 8;

/// 恒定时间比较（长度 + 逐字节 XOR，避免短路时序）。
fn constant_time_eq(expected: &str, given: &str) -> bool {
    if expected.len() != given.len() {
        return false;
    }
    expected
        .bytes()
        .zip(given.bytes())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

pub struct ServerPassword {
    store: Arc<Store>,
    default: Option<String>,
}

impl ServerPassword {
    pub fn new(store: Arc<Store>, default: Option<String>) -> Self {
        Self {
            store,
            default: default
                .filter(|p| !p.trim().is_empty())
                .map(|p| p.trim().to_string()),
        }
    }
}

impl AdminPassword for ServerPassword {
    fn available(&self) -> bool {
        self.store.has_password() || self.default.is_some()
    }

    fn custom_set(&self) -> bool {
        self.store.has_password()
    }

    fn verify(&self, given: &str) -> bool {
        if self.store.has_password() {
            self.store.verify_password(given)
        } else {
            self.default
                .as_deref()
                .is_some_and(|expected| constant_time_eq(expected, given))
        }
    }

    fn set_custom(&self, given: &str) -> Result<(), String> {
        if given.trim().is_empty() {
            return Err("口令不能为空".to_string());
        }
        if given.chars().count() < MIN_PASSWORD_LEN {
            return Err(format!("管理口令至少 {MIN_PASSWORD_LEN} 个字符"));
        }
        self.store.set_password(given).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pw(default: Option<&str>) -> ServerPassword {
        ServerPassword::new(
            Arc::new(Store::open_in_memory().expect("内存库")),
            default.map(str::to_string),
        )
    }

    #[test]
    fn default_password_verify_and_available() {
        let p = pw(Some("s3cret"));
        assert!(p.available());
        assert!(!p.custom_set(), "未写库 = 仍是默认口令");
        assert!(p.verify("s3cret"));
        assert!(!p.verify("nope"));
    }

    #[test]
    fn no_password_is_readonly() {
        let p = pw(None);
        assert!(!p.available(), "无口令 → 只读");
        assert!(!p.verify(""));
    }

    #[test]
    fn set_custom_shadows_default() {
        let p = pw(Some("old-default"));
        assert!(p.verify("old-default"));
        p.set_custom("brand-new-pass").expect("写入");
        assert!(p.custom_set());
        assert!(p.verify("brand-new-pass"));
        assert!(!p.verify("old-default"), "库内口令存在时 env 默认被遮蔽");
        assert!(p.set_custom("short").is_err(), "过短口令拒绝");
    }

    #[test]
    fn custom_password_survives_reopen_via_store() {
        let store = Arc::new(Store::open_in_memory().expect("内存库"));
        ServerPassword::new(store.clone(), None)
            .set_custom("keep-me-please")
            .expect("设置");
        let p2 = ServerPassword::new(store, Some("env-fallback".to_string()));
        assert!(p2.custom_set());
        assert!(p2.verify("keep-me-please"));
        assert!(!p2.verify("env-fallback"), "默认被遮蔽");
    }
}
