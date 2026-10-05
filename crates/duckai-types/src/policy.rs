//! 单出口（egress）冷却/分发策略——按 direct / 每条代理分别配置并持久化。
//!
//! 常量与既有 §6.1 缺省完全一致（改策略 = 改字段，不改行为基线）：
//! `cooldown_enabled=false` 时该出口**失败不进入冷却**（仅健康分降序，仍参与分发）；
//! `enabled=false` 时该出口整体停用（不参与分发与探测，快照仍可见可再启用）。

use serde::{Deserialize, Serialize};

/// 首次封禁冷却时长（秒）——418 持久型，到期后半开探测。
pub const INITIAL_BAN_SECS: u64 = 5 * 60;
/// 冷却倍增上限（24 小时，§6.1「上限 24h」）。
pub const BAN_CAP_SECS: u64 = 24 * 60 * 60;
/// 429 退避上限（10 分钟，§6.1）。
pub const COOLDOWN_CAP_SECS: u64 = 10 * 60;
/// 429 无 Retry-After 时的起始退避（5s，指数倍增）。
pub const RATE_LIMIT_BASE_SECS: u64 = 5;

/// 单出口策略（管理面可编辑，落 `egress` 表，重启后仍生效）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EgressPolicy {
    /// 出口启用（false = 不参与分发/探测）。
    pub enabled: bool,
    /// 失败是否触发冷却（false = 418/429 不降级状态，只扣健康分）。
    pub cooldown_enabled: bool,
    /// 首次封禁秒数（倍增底）。
    pub ban_secs: u64,
    /// 封禁秒数上限。
    pub ban_cap_secs: u64,
    /// 429 无 Retry-After 的起始退避秒数。
    pub rate_limit_secs: u64,
}

impl Default for EgressPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            cooldown_enabled: true,
            ban_secs: INITIAL_BAN_SECS,
            ban_cap_secs: BAN_CAP_SECS,
            rate_limit_secs: RATE_LIMIT_BASE_SECS,
        }
    }
}

impl EgressPolicy {
    /// 参数合法性（管理面入口统一校验）：秒数 ≥ 1 且 cap ≥ 首值。
    pub fn validate(&self) -> Result<(), String> {
        if self.ban_secs == 0 {
            return Err("封禁秒数必须 ≥ 1".to_string());
        }
        if self.ban_cap_secs < self.ban_secs {
            return Err("封禁上限秒数不能小于首次封禁秒数".to_string());
        }
        if self.rate_limit_secs == 0 {
            return Err("429 退避起始秒数必须 ≥ 1".to_string());
        }
        if self.ban_cap_secs > BAN_CAP_SECS {
            return Err(format!("封禁上限不得超过 {} 秒（24h）", BAN_CAP_SECS));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_s61_baseline() {
        let p = EgressPolicy::default();
        assert!(p.enabled && p.cooldown_enabled);
        assert_eq!(p.ban_secs, 5 * 60);
        assert_eq!(p.ban_cap_secs, 24 * 60 * 60);
        assert_eq!(p.rate_limit_secs, 5);
        assert!(p.validate().is_ok());
    }

    #[test]
    fn validation_rejects_zero_and_inverted() {
        let p = EgressPolicy {
            ban_secs: 0,
            ..EgressPolicy::default()
        };
        assert!(p.validate().is_err());
        let p = EgressPolicy {
            ban_secs: 60,
            ban_cap_secs: 10,
            ..EgressPolicy::default()
        };
        assert!(p.validate().is_err());
        let p = EgressPolicy {
            rate_limit_secs: 0,
            ..EgressPolicy::default()
        };
        assert!(p.validate().is_err());
        let p = EgressPolicy {
            ban_cap_secs: BAN_CAP_SECS + 1,
            ..EgressPolicy::default()
        };
        assert!(p.validate().is_err());
    }

    #[test]
    fn serde_roundtrip_for_admin_api() {
        let p = EgressPolicy {
            enabled: false,
            cooldown_enabled: false,
            ban_secs: 60,
            ban_cap_secs: 3600,
            rate_limit_secs: 30,
        };
        let json = serde_json::to_string(&p).expect("序列化");
        let back: EgressPolicy = serde_json::from_str(&json).expect("反序列化");
        assert_eq!(p, back);
    }
}
