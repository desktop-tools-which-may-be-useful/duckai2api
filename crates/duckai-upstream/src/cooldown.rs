//! 封禁冷却状态机（ARCHITECTURE §6.1）。
//!
//! 一个 egress = 直连或一条代理。状态迁移（时间戳 + 原因码，内存持久化，重启清零可接受）：
//!
//! ```text
//! Healthy ──418 ERR_BN_LIMIT──► Banned ──冷却到期──► HalfOpen(探测1次) ──成功──► Healthy
//!    │                           ▲                        └──失败──► Banned(冷却×2，上限 24h)
//!    ├──429(retry-after)──► Cooldown ──到期──► Healthy
//!    └──挑战失效：不换 egress（由 VqdStore 重取，≤3）
//! ```
//!
//! 要点：
//! - **418 持久型（无 Retry-After）**→ 直接 `Banned`（长冷却），换端重放由代理池处理；
//! - **429** → 尊重 `Retry-After`，缺省指数退避（5s→10s→…，上限 10min）→ `Cooldown`；
//! - 挑战类错误**不经过**本状态机（不消耗 egress）；
//! - `Cooldown` / `Banned` / `HalfOpen` 不参与常规分发（§6.2）。

use std::time::Duration;

use duckai_types::EgressPolicy;

pub use duckai_types::policy::{
    BAN_CAP_SECS, COOLDOWN_CAP_SECS, INITIAL_BAN_SECS, RATE_LIMIT_BASE_SECS,
};

/// 单出口状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EgressState {
    Healthy,
    Cooldown,
    Banned,
    /// 冷却到期后的半开探测态：仅允许 1 次探测（由代理池限制 inflight）。
    HalfOpen,
}

impl EgressState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Healthy => "Healthy",
            Self::Cooldown => "Cooldown",
            Self::Banned => "Banned",
            Self::HalfOpen => "HalfOpen",
        }
    }
}

/// 状态 + 时间戳 + 原因码（WebUI 时间线渲染单元）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressStatus {
    pub state: EgressState,
    /// 冷却截止（epoch ms；0 = 无冷却）。
    pub until_ms: u64,
    /// 迁移原因码，如 `418 ERR_BN_LIMIT` / `429 retry-after=7` / `success`。
    pub reason: String,
    /// 连续失败计数（429 退避指数的底）。
    pub consecutive_failures: u32,
    /// 累计封禁轮次（半开失败 ×2 的指数底）。
    pub ban_rounds: u32,
    /// 最近一次迁移时间（epoch ms）。
    pub since_ms: u64,
}

/// 单出口状态机（由 `EgressPool` 每出口持有一个，内部可变）。
#[derive(Debug)]
pub struct EgressMachine {
    status: EgressStatus,
    /// 该出口的可配置策略（冷却开关/封禁时长/停用），默认 = §6.1 基线常量。
    policy: EgressPolicy,
}

impl Default for EgressMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl EgressMachine {
    pub fn new() -> Self {
        Self::with_policy(EgressPolicy::default())
    }

    /// 指定策略构建（池启动时由 `egress` 表注入）。
    pub fn with_policy(policy: EgressPolicy) -> Self {
        Self {
            status: EgressStatus {
                state: EgressState::Healthy,
                until_ms: 0,
                reason: "init".to_string(),
                consecutive_failures: 0,
                ban_rounds: 0,
                since_ms: 0,
            },
            policy,
        }
    }

    /// 当前策略（分发开关与快照渲染读取）。
    pub fn policy(&self) -> &EgressPolicy {
        &self.policy
    }

    /// 运行时更新策略：关闭冷却即把非健康态清回分发池（策略即开关）。
    pub fn set_policy(&mut self, policy: EgressPolicy) {
        self.policy = policy;
        if !policy.cooldown_enabled && !matches!(self.status.state, EgressState::Healthy) {
            self.status.state = EgressState::Healthy;
            self.status.until_ms = 0;
            self.status.reason = "cooldown disabled".to_string();
            self.status.since_ms = now_ms();
        }
    }

    /// 到期迁移（幂等），返回当前状态快照。
    pub fn status(&mut self, now_ms: u64) -> EgressStatus {
        self.refresh(now_ms);
        self.status.clone()
    }

    fn refresh(&mut self, now_ms: u64) {
        if self.status.until_ms == 0 || now_ms < self.status.until_ms {
            return;
        }
        match self.status.state {
            EgressState::Cooldown => {
                // 429 冷却到期 → 回到分发池（半开语义：连续失败清零由成功事件负责）
                self.status.state = EgressState::Healthy;
                self.status.until_ms = 0;
                self.status.reason = "cooldown expired".to_string();
                self.status.since_ms = now_ms;
            }
            EgressState::Banned => {
                // 封禁到期 → 半开（仅允许探测 1 次）
                self.status.state = EgressState::HalfOpen;
                self.status.until_ms = 0;
                self.status.reason = "ban cooldown expired (half-open)".to_string();
                self.status.since_ms = now_ms;
            }
            EgressState::Healthy | EgressState::HalfOpen => {
                self.status.until_ms = 0;
            }
        }
    }

    /// 是否可参与常规分发（§6.2：Cooldown/Banned/HalfOpen 不参与）。
    pub fn is_dispatchable(&mut self, now_ms: u64) -> bool {
        self.refresh(now_ms);
        self.status.state == EgressState::Healthy
    }

    /// 半开态可用于探测（池侧再限制 inflight==1）。
    pub fn is_probe_candidate(&mut self, now_ms: u64) -> bool {
        self.refresh(now_ms);
        self.status.state == EgressState::HalfOpen
    }

    /// 成功 chat → Healthy，清零连续失败与封禁轮次。
    pub fn on_success(&mut self, now_ms: u64) {
        self.status = EgressStatus {
            state: EgressState::Healthy,
            until_ms: 0,
            reason: "success".to_string(),
            consecutive_failures: 0,
            ban_rounds: 0,
            since_ms: now_ms,
        };
    }

    /// 429 → Cooldown：尊重 Retry-After，缺省按策略起始退避指数倍增，上限 10min。
    /// 策略关闭冷却时仅扣连续失败计数，状态保持 Healthy（该出口失败不降级）。
    pub fn on_rate_limited(&mut self, now_ms: u64, retry_after_secs: Option<u64>) {
        self.refresh(now_ms);
        let failures = self.status.consecutive_failures;
        self.status.consecutive_failures = failures + 1;
        if !self.policy.cooldown_enabled {
            if self.status.state != EgressState::Healthy {
                self.status.state = EgressState::Healthy;
                self.status.until_ms = 0;
            }
            self.status.reason = "429 ignored (cooldown disabled)".to_string();
            self.status.since_ms = now_ms;
            return;
        }
        let secs = match retry_after_secs {
            Some(s) if s > 0 => s,
            _ => self
                .policy
                .rate_limit_secs
                .saturating_mul(1u64 << failures.min(8))
                .min(COOLDOWN_CAP_SECS),
        }
        .min(COOLDOWN_CAP_SECS);
        let reason = match retry_after_secs {
            Some(s) => format!("429 retry-after={s}"),
            None => format!("429 backoff={secs}s"),
        };
        self.status.state = EgressState::Cooldown;
        self.status.until_ms = now_ms.saturating_add(secs.saturating_mul(1000));
        self.status.reason = reason;
        self.status.since_ms = now_ms;
    }

    /// 418 ERR_BN_LIMIT 等持久型受限 → 直接 Banned（长冷却，轮次倍增，
    /// 首值/上限取该出口策略）。策略关闭冷却时仅记录原因，状态不降级。
    pub fn on_banned(&mut self, now_ms: u64, reason: &str) {
        self.refresh(now_ms);
        self.status.ban_rounds = self.status.ban_rounds.saturating_add(1);
        self.status.consecutive_failures = self.status.consecutive_failures.saturating_add(1);
        if !self.policy.cooldown_enabled {
            if self.status.state != EgressState::Healthy {
                self.status.state = EgressState::Healthy;
                self.status.until_ms = 0;
            }
            self.status.reason = format!("{reason} (cooldown disabled)");
            self.status.since_ms = now_ms;
            return;
        }
        self.apply_ban(now_ms, reason);
    }

    /// 封禁状态落库到本机（计数已由调用方递增）：首值按轮次倍增，封顶策略上限。
    fn apply_ban(&mut self, now_ms: u64, reason: &str) {
        let rounds = self.status.ban_rounds.saturating_sub(1);
        let secs = self
            .policy
            .ban_secs
            .saturating_mul(1u64 << rounds.min(6))
            .min(self.policy.ban_cap_secs);
        self.status.state = EgressState::Banned;
        self.status.until_ms = now_ms.saturating_add(secs.saturating_mul(1000));
        self.status.reason = reason.to_string();
        self.status.since_ms = now_ms;
    }

    /// 半开探测成功 → 完全恢复。
    pub fn on_probe_success(&mut self, now_ms: u64) {
        self.on_success(now_ms);
        self.status.reason = "probe success".to_string();
    }

    /// 半开探测失败 → 再次 Banned（冷却 ×2，上限 24h）。
    pub fn on_probe_failure(&mut self, now_ms: u64, reason: &str) {
        self.on_banned(now_ms, reason);
        self.status.reason = format!("probe failed: {reason}");
    }

    /// 运维手动解封（P1-6：banned 不再只靠重启恢复）。
    pub fn admin_unban(&mut self, now_ms: u64) {
        self.status = EgressStatus {
            state: EgressState::Healthy,
            until_ms: 0,
            reason: "admin unban".to_string(),
            consecutive_failures: 0,
            ban_rounds: 0,
            since_ms: now_ms,
        };
    }

    /// 运维手动封禁：显式后门，**绕过**策略的冷却开关（管理员说了算）。
    pub fn admin_ban(&mut self, now_ms: u64) {
        self.refresh(now_ms);
        self.status.ban_rounds = self.status.ban_rounds.saturating_add(1);
        self.status.consecutive_failures = self.status.consecutive_failures.saturating_add(1);
        self.apply_ban(now_ms, "admin ban");
    }

    /// 距离冷却结束的剩余秒数（0 = 无冷却）。
    pub fn remaining_secs(&self, now_ms: u64) -> u64 {
        self.status.until_ms.saturating_sub(now_ms).div_ceil(1000)
    }
}

/// 便捷：毫秒时间戳。
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000;

    /// 表驱动：§6.1 状态迁移矩阵。
    #[test]
    fn transition_table() {
        // (动作序列, 期望状态)
        type Case = (&'static str, Box<dyn Fn(&mut EgressMachine)>, EgressState);
        let cases: Vec<Case> = vec![
            (
                "418 → Banned",
                Box::new(|m| m.on_banned(0, "418 ERR_BN_LIMIT")),
                EgressState::Banned,
            ),
            (
                "429 + Retry-After=7 → Cooldown(7s)",
                Box::new(|m| m.on_rate_limited(0, Some(7))),
                EgressState::Cooldown,
            ),
            (
                "429 无 Retry-After → Cooldown(5s)",
                Box::new(|m| m.on_rate_limited(0, None)),
                EgressState::Cooldown,
            ),
        ];
        for (name, action, want) in cases {
            let mut m = EgressMachine::new();
            action(&mut m);
            assert_eq!(m.status(0).state, want, "{name}");
        }

        // Retry-After 精确生效
        let mut m = EgressMachine::new();
        m.on_rate_limited(0, Some(7));
        assert_eq!(m.status(0).until_ms, 7 * MS);

        // 429 无 Retry-After 指数：5 → 10 → 20 …，上限 600
        let mut m = EgressMachine::new();
        m.on_rate_limited(0, None);
        assert_eq!(m.status(0).until_ms, 5 * MS);
        m.on_success(6 * MS);
        m.on_rate_limited(6 * MS, None);
        assert_eq!(m.status(6 * MS).until_ms, 6 * MS + 5 * MS);
        let mut m = EgressMachine::new();
        m.on_rate_limited(0, Some(10_000)); // 超上限截断 600s
        assert_eq!(m.status(0).until_ms, COOLDOWN_CAP_SECS * MS);
    }

    #[test]
    fn cooldown_expiry_returns_to_healthy() {
        let mut m = EgressMachine::new();
        m.on_rate_limited(0, Some(7));
        assert!(!m.is_dispatchable(6 * MS));
        assert!(m.is_dispatchable(7 * MS));
        assert_eq!(m.status(7 * MS).state, EgressState::Healthy);
    }

    #[test]
    fn ban_expiry_goes_half_open_then_recovers() {
        let mut m = EgressMachine::new();
        m.on_banned(0, "418 ERR_BN_LIMIT");
        let until = INITIAL_BAN_SECS * MS;
        assert!(!m.is_dispatchable(until - 1));
        assert!(!m.is_dispatchable(until), "Banned 不参与常规分发");
        assert!(m.is_probe_candidate(until), "到期 → HalfOpen 探测");
        m.on_probe_success(until);
        assert!(m.is_dispatchable(until));
        assert_eq!(m.status(until).state, EgressState::Healthy);
    }

    #[test]
    fn probe_failure_doubles_ban_rounds_capped_at_24h() {
        let mut m = EgressMachine::new();
        m.on_banned(0, "418");
        let t1 = INITIAL_BAN_SECS * MS;
        m.on_probe_failure(t1, "still banned");
        let until2 = t1 + INITIAL_BAN_SECS * 2 * MS;
        assert_eq!(m.status(t1).until_ms, until2);
        // 反复失败 → 倍增封顶 24h
        for _ in 0..10 {
            let now = m.status(until2).until_ms;
            m.on_banned(now, "418");
        }
        let st = m.status(u64::MAX / 4);
        let dur = st.until_ms.saturating_sub(u64::MAX / 4);
        assert!(dur <= BAN_CAP_SECS * MS, "封禁时长 {dur}ms 必须 ≤ 24h");
    }

    #[test]
    fn success_resets_everything() {
        let mut m = EgressMachine::new();
        m.on_banned(0, "418");
        m.admin_unban(1);
        m.on_rate_limited(2, Some(3));
        m.on_success(10 * MS);
        let st = m.status(10 * MS);
        assert_eq!(st.state, EgressState::Healthy);
        assert_eq!(st.consecutive_failures, 0);
        assert_eq!(st.ban_rounds, 0);
        assert_eq!(st.until_ms, 0);
    }

    // ---- per-egress 策略 ----

    #[test]
    fn policy_cooldown_disabled_never_transitions() {
        let p = EgressPolicy {
            cooldown_enabled: false,
            ..EgressPolicy::default()
        };
        let mut m = EgressMachine::with_policy(p);
        m.on_banned(0, "418");
        let st = m.status(0);
        assert_eq!(st.state, EgressState::Healthy, "关闭冷却 → 失败不降级");
        assert_eq!(st.until_ms, 0);
        assert!(st.reason.contains("cooldown disabled"), "{}", st.reason);
        m.on_rate_limited(1_000, None);
        let st = m.status(1_000);
        assert_eq!(st.state, EgressState::Healthy);
        assert!(
            st.consecutive_failures >= 1,
            "计数仍累计（健康分排序依赖它）"
        );
    }

    #[test]
    fn policy_custom_ban_duration_and_cap() {
        let p = EgressPolicy {
            ban_secs: 30,
            ban_cap_secs: 60,
            ..EgressPolicy::default()
        };
        let mut m = EgressMachine::with_policy(p);
        m.on_banned(0, "418");
        assert_eq!(m.status(0).until_ms, 30 * MS, "首封 30s");
        m.on_banned(30 * MS, "418");
        assert_eq!(
            m.status(30 * MS).until_ms,
            30 * MS + 60 * MS,
            "倍增 60s 后封顶 cap"
        );
    }

    #[test]
    fn set_policy_disable_clears_active_cooldown() {
        let mut m = EgressMachine::new();
        m.on_banned(0, "418");
        assert_eq!(m.status(0).state, EgressState::Banned);
        let mut p = *m.policy();
        p.cooldown_enabled = false;
        m.set_policy(p);
        assert_eq!(m.status(0).state, EgressState::Healthy, "关冷却即解封");
    }

    #[test]
    fn admin_ban_bypasses_cooldown_switch() {
        let p = EgressPolicy {
            cooldown_enabled: false,
            ..EgressPolicy::default()
        };
        let mut m = EgressMachine::with_policy(p);
        m.admin_ban(0);
        assert_eq!(
            m.status(0).state,
            EgressState::Banned,
            "手动封禁是显式后门，绕过策略开关"
        );
    }

    #[test]
    fn rate_limit_uses_policy_base() {
        let p = EgressPolicy {
            rate_limit_secs: 30,
            ..EgressPolicy::default()
        };
        let mut m = EgressMachine::with_policy(p);
        m.on_rate_limited(0, None);
        assert_eq!(m.status(0).until_ms, 30 * MS, "按策略起始退避");
    }
}
