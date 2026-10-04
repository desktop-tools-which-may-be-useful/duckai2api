//! VQD 生命周期状态机（§5.4 求解链的缓存/失效/重试计数）。
//!
//! 本模块**不做 I/O**：取挑战与发问由 `duckai-upstream` 驱动，这里只维护
//! 「覆盖令牌 → 缓存令牌 → 失效 → 挑战重试计数」的纯状态，供上游层在发问前查询、
//! 在 418/挑战失败后推进。

use std::sync::RwLock;

/// 令牌缓存 TTL（10 分钟，与 `DUCKAI_VQD_OVERRIDE` 注释一致；过期后重新取挑战）。
pub const VQD_TTL_MS: u64 = 10 * 60 * 1000;
/// 挑战失败连续上限：超过后上游层应切换 override → browser → 503（§5.4）。
pub const VQD_MAX_ATTEMPTS: u8 = 3;

#[derive(Debug)]
struct Inner {
    token: Option<String>,
    fetched_at_ms: u64,
    attempts: u8,
}

/// VQD 状态缓存。
#[derive(Debug)]
pub struct VqdStore {
    inner: RwLock<Inner>,
    override_token: Option<String>,
    override_expiry_ms: Option<u64>,
    ttl_ms: u64,
    max_attempts: u8,
}

impl VqdStore {
    /// 创建缓存。`override_token` 来自 `DUCKAI_VQD_OVERRIDE`，带 10 分钟 TTL（override 优先）。
    pub fn new(override_token: Option<String>) -> Self {
        Self {
            inner: RwLock::new(Inner {
                token: None,
                fetched_at_ms: 0,
                attempts: 0,
            }),
            override_token,
            override_expiry_ms: None,
            ttl_ms: VQD_TTL_MS,
            max_attempts: VQD_MAX_ATTEMPTS,
        }
    }

    /// 带覆盖 TTL 的构造（测试注入 `now_ms` + 自定义过期）。
    pub fn with_override_at(override_token: Option<String>, now_ms: u64) -> Self {
        let expiry = override_token.as_ref().map(|_| now_ms + VQD_TTL_MS);
        let mut store = Self::new(override_token);
        store.override_expiry_ms = expiry;
        store
    }

    /// 当前可用令牌：未过期的 override 优先，其次是缓存。
    pub fn token(&self, now_ms: u64) -> Option<String> {
        if let Some(exp) = self.override_expiry_ms {
            if now_ms < exp {
                return self.override_token.clone();
            }
        } else if self.override_token.is_some() {
            return self.override_token.clone();
        }
        let inner = self.inner.read().ok()?;
        let token = inner.token.clone()?;
        (now_ms.saturating_sub(inner.fetched_at_ms) <= self.ttl_ms).then_some(token)
    }

    /// 缓存新令牌并清零挑战失败计数。
    pub fn store(&self, token: String, now_ms: u64) {
        if let Ok(mut inner) = self.inner.write() {
            inner.token = Some(token);
            inner.fetched_at_ms = now_ms;
            inner.attempts = 0;
        }
    }

    /// 令牌失效（418 / 挑战错误后调用；下一次发问重新取挑战）。
    pub fn invalidate(&self) {
        if let Ok(mut inner) = self.inner.write() {
            inner.token = None;
            inner.fetched_at_ms = 0;
        }
    }

    /// 记一次挑战失败，返回**本次连续失败次数**（达到 [`VQD_MAX_ATTEMPTS`] 后应升级求解链）。
    pub fn note_challenge_failure(&self) -> u8 {
        if let Ok(mut inner) = self.inner.write() {
            inner.attempts = inner.attempts.saturating_add(1);
            inner.attempts
        } else {
            1
        }
    }

    /// 当前连续挑战失败次数。
    pub fn challenge_attempts(&self) -> u8 {
        self.inner.read().map(|i| i.attempts).unwrap_or(0)
    }

    /// 连续挑战失败是否已达上限（上游层据此升级求解链：override → browser → 503，§5.4）。
    pub fn challenge_exhausted(&self) -> bool {
        self.challenge_attempts() >= self.max_attempts
    }

    /// 是否已有未过期令牌（`/admin` 健康位用）。
    pub fn has_valid_token(&self, now_ms: u64) -> bool {
        self.token(now_ms).is_some()
    }

    /// 从 `X-Vqd-Hash-1` 响应头取挑战（「取挑战」的解析侧）。
    pub fn extract_challenge(header_value: &str) -> Option<&str> {
        let v = header_value.trim();
        (!v.is_empty()).then_some(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_ttl_and_invalidate() {
        let s = VqdStore::new(None);
        assert_eq!(s.token(1000), None);
        s.store("tok".into(), 1000);
        assert_eq!(s.token(1000), Some("tok".into()));
        assert_eq!(s.token(1000 + VQD_TTL_MS), Some("tok".into()));
        assert_eq!(s.token(1000 + VQD_TTL_MS + 1), None, "TTL 过期");
        s.store("tok2".into(), 5000);
        assert_eq!(s.token(6000), Some("tok2".into()));
        s.invalidate();
        assert_eq!(s.token(6000), None);
    }

    #[test]
    fn override_wins_and_expires() {
        let s = VqdStore::with_override_at(Some("ovr".into()), 0);
        s.store("cached".into(), 0);
        assert_eq!(s.token(1), Some("ovr".into()), "override 优先");
        assert_eq!(
            s.token(VQD_TTL_MS),
            Some("cached".into()),
            "override 过期回落缓存"
        );
    }

    #[test]
    fn no_override_key_present_but_disabled() {
        let s = VqdStore::new(None);
        s.store("cached".into(), 0);
        assert_eq!(s.token(10), Some("cached".into()));
    }

    #[test]
    fn challenge_failure_counter_capped_flow() {
        let s = VqdStore::new(None);
        assert_eq!(s.note_challenge_failure(), 1);
        assert_eq!(s.note_challenge_failure(), 2);
        assert_eq!(s.note_challenge_failure(), 3);
        assert_eq!(s.challenge_attempts(), 3);
        s.store("tok".into(), 0);
        assert_eq!(s.challenge_attempts(), 0, "取到新令牌后归零");
        assert!(!s.challenge_exhausted());
        s.note_challenge_failure();
        s.note_challenge_failure();
        s.note_challenge_failure();
        assert!(s.challenge_exhausted(), "达 3 次 → 升级求解链");
    }

    #[test]
    fn extract_challenge() {
        assert_eq!(VqdStore::extract_challenge("  abc  "), Some("abc"));
        assert_eq!(VqdStore::extract_challenge("   "), None);
    }
}
