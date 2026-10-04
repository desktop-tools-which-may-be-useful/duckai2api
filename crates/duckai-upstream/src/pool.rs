//! 代理池（ARCHITECTURE §6.2）：健康分 + 会话粘性 + 换端选择 + 运维后门。
//!
//! - 配置：`DUCKAI_PROXIES`（逗号分隔，兼容 `DUCKAI_PROXY`）；`http/https/socks5`；
//!   为空 = 直连 egress。配置了代理则**只**走代理（不泄漏直连）。
//! - 选择：`Healthy` 集合按滑动窗口健康分（成功 +1 / 429 −2 / 418 −10）降序 +
//!   同会话粘性（`session_hint` 指纹 → 同一出口）挑选；`Cooldown`/`Banned`/`HalfOpen`
//!   不参与常规分发；全部不可用时 `HalfOpen` 出口允许探测（inflight ≤ 1）。
//! - 传输层真正生效：`acquire()` 返回的代理 URL 由 http/browser 适配器直接配置到
//!   reqwest（`Proxy::all`），非死配置（P0-3 修正位，wiremock 集成测试断言）。
//! - 并发：每 egress inflight 上限 [`PER_EGRESS_LIMIT`]（§6.1，全局闸门在 API 层）。

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use duckai_types::error::EgressScope;
use duckai_types::snapshot::EgressSnapshot;
use thiserror::Error;

use crate::cooldown::{EgressMachine, EgressState, now_ms};

/// 单 egress 并发上限（§6.1）。
pub const PER_EGRESS_LIMIT: usize = 2;
/// 健康分滑动窗口长度。
const SCORE_WINDOW: usize = 32;
pub const SCORE_SUCCESS: i32 = 1;
pub const SCORE_RATE_LIMIT: i32 = -2;
pub const SCORE_BANNED: i32 = -10;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PoolError {
    #[error("invalid proxy {url:?}: {msg}")]
    InvalidProxy { url: String, msg: String },
}

/// 合并 `DUCKAI_PROXIES`（逗号分隔）与遗留 `DUCKAI_PROXY`，去空、去重、校验 scheme。
pub fn parse_proxy_config(
    proxies: Option<&str>,
    single: Option<&str>,
) -> Result<Vec<String>, PoolError> {
    let mut out: Vec<String> = Vec::new();
    for raw in proxies
        .unwrap_or("")
        .split(',')
        .chain(single.unwrap_or("").split(','))
    {
        let url = raw.trim();
        if url.is_empty() {
            continue;
        }
        validate_proxy(url)?;
        if !out.iter().any(|e| e == url) {
            out.push(url.to_string());
        }
    }
    Ok(out)
}

fn validate_proxy(url: &str) -> Result<(), PoolError> {
    let bad = |msg: &str| PoolError::InvalidProxy {
        url: url.to_string(),
        msg: msg.to_string(),
    };
    let (scheme, rest) = url.split_once("://").ok_or_else(|| bad("缺少 scheme"))?;
    match scheme.to_ascii_lowercase().as_str() {
        "http" | "https" | "socks5" => {}
        other => {
            return Err(bad(&format!(
                "不支持的 scheme {other:?}（期望 http/https/socks5）"
            )));
        }
    }
    let host = rest.rsplit('@').next().unwrap_or(rest);
    let host = host.split('/').next().unwrap_or(host);
    if host.is_empty() {
        return Err(bad("主机为空"));
    }
    Ok(())
}

/// 脱敏代理 URL（仅 `scheme://host[:port]`，剥离 `user:pass@` 凭据与 path）——对外快照专用。
pub fn sanitize_proxy_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let authority = rest.rsplit('@').next().unwrap_or(rest);
    let host = authority.split('/').next().unwrap_or(authority);
    format!("{scheme}://{host}")
}

/// 一次分发句柄：适配器用它选传输层代理并在事后回记状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressHandle {
    /// 出口序号（= `EgressScope` 里的下标）。
    pub index: usize,
    pub scope: EgressScope,
    /// `None` = 直连；`Some` = 代理 URL（原样配置到 reqwest）。
    pub proxy_url: Option<String>,
}

struct Slot {
    proxy_url: Option<String>,
    machine: Mutex<EgressMachine>,
    score: Mutex<VecDeque<i32>>,
    inflight: AtomicUsize,
}

/// 代理池：直连或一组代理，每个出口带独立状态机与健康分。
///
/// 槽位表可经 [`EgressPool::reconfigure`] 运行时更换（管理面「增删代理」），
/// 故置于 `RwLock` 之后；读路径（分发/回记/快照）取读锁，仅 reconfigure 取写锁。
pub struct EgressPool {
    slots: std::sync::RwLock<Vec<Slot>>,
    /// session_hint 指纹 → 出口序号（同会话粘性）。
    sticky: Mutex<HashMap<String, usize>>,
}

impl std::fmt::Debug for EgressPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let n = self.with_slots(|s| s.len());
        f.debug_struct("EgressPool").field("egresses", &n).finish()
    }
}

fn new_slot(proxy_url: Option<String>) -> Slot {
    Slot {
        proxy_url,
        machine: Mutex::new(EgressMachine::new()),
        score: Mutex::new(VecDeque::new()),
        inflight: AtomicUsize::new(0),
    }
}

impl EgressPool {
    /// `proxies` 为空 → 单一直连出口；非空 → 仅这些代理（不泄漏直连）。
    pub fn new(proxies: &[String]) -> Result<Self, PoolError> {
        for p in proxies {
            validate_proxy(p)?;
        }
        let slots: Vec<Slot> = if proxies.is_empty() {
            vec![new_slot(None)]
        } else {
            proxies.iter().map(|p| new_slot(Some(p.clone()))).collect()
        };
        Ok(Self {
            slots: std::sync::RwLock::new(slots),
            sticky: Mutex::new(HashMap::new()),
        })
    }

    /// 持读锁执行（中毒时取内值）；读路径统一入口。
    fn with_slots<T>(&self, f: impl FnOnce(&[Slot]) -> T) -> T {
        let guard = self.slots.read().unwrap_or_else(|e| e.into_inner());
        f(&guard)
    }

    pub fn len(&self) -> usize {
        self.with_slots(|slots| slots.len())
    }

    pub fn is_empty(&self) -> bool {
        self.with_slots(|slots| slots.is_empty())
    }

    /// 原始代理 URL 列表（管理面内部用；对外快照一律经 [`sanitize_proxy_url`])。
    pub fn proxy_urls(&self) -> Vec<String> {
        self.with_slots(|slots| slots.iter().filter_map(|s| s.proxy_url.clone()).collect())
    }

    /// 当前可分发（Healthy）的出口数。
    pub fn healthy_count(&self) -> usize {
        let now = now_ms();
        self.with_slots(|slots| {
            slots
                .iter()
                .filter(|s| self.with_machine(s, |m| m.is_dispatchable(now)))
                .count()
        })
    }

    /// 当前 Banned 的出口数。
    pub fn banned_count(&self) -> usize {
        let now = now_ms();
        self.with_slots(|slots| {
            slots
                .iter()
                .filter(|s| {
                    self.with_machine(s, |m| matches!(m.status(now).state, EgressState::Banned))
                })
                .count()
        })
    }

    /// 按健康分（降序）+ 会话粘性选一条出口；无可用时半开探测；再无则 `None`。
    pub fn acquire(&self, session_hint: Option<&str>) -> Option<EgressHandle> {
        let now = now_ms();
        let fp = session_hint.map(duckai_protocol::chat::session_fingerprint);

        self.with_slots(|slots| {
            // 1) 健康集合（Healthy 且未达并发上限）
            let candidates: Vec<usize> = slots
                .iter()
                .enumerate()
                .filter(|(_, s)| {
                    s.inflight.load(Ordering::SeqCst) < PER_EGRESS_LIMIT
                        && self.with_machine(s, |m| m.is_dispatchable(now))
                })
                .map(|(i, _)| i)
                .collect();

            // 2) 同会话粘性命中且仍健康 → 复用同一条
            if let Some(fp) = &fp {
                if let Some(idx) = self.sticky.lock().ok().and_then(|st| st.get(fp).copied()) {
                    if candidates.contains(&idx) {
                        return self.take(slots, idx, Some(fp.clone()));
                    }
                }
            }

            // 3) 健康分最高的出口（并列取序号小者）
            if let Some(&best) = candidates
                .iter()
                .max_by_key(|i| (score_in(slots, **i), std::cmp::Reverse(**i)))
            {
                return self.take(slots, best, fp.clone());
            }

            // 4) 全部不可用 → HalfOpen 出口允许探测 1 次（inflight==0）
            let probe = slots
                .iter()
                .enumerate()
                .find(|(_, s)| {
                    s.inflight.load(Ordering::SeqCst) == 0
                        && self.with_machine(s, |m| m.is_probe_candidate(now))
                })
                .map(|(i, _)| i)?;
            self.take(slots, probe, fp)
        })
    }

    fn take(&self, slots: &[Slot], idx: usize, fp: Option<String>) -> Option<EgressHandle> {
        let slot = slots.get(idx)?;
        slot.inflight.fetch_add(1, Ordering::SeqCst);
        if let Some(fp) = fp {
            if let Ok(mut st) = self.sticky.lock() {
                st.insert(fp, idx);
            }
        }
        Some(EgressHandle {
            index: idx,
            scope: scope_of(idx, &slot.proxy_url),
            proxy_url: slot.proxy_url.clone(),
        })
    }

    /// 归还并发额度（每次 acquire 之后必须恰好一次）。
    pub fn release(&self, handle: &EgressHandle) {
        self.with_slots(|slots| {
            if let Some(slot) = slots.get(handle.index) {
                let prev = slot.inflight.fetch_sub(1, Ordering::SeqCst);
                debug_assert!(prev > 0, "release 未配对的 inflight");
                if prev == 0 {
                    slot.inflight.store(0, Ordering::SeqCst);
                }
            }
        });
    }

    /// 成功 chat → 健康分 +1、状态机回 Healthy。
    pub fn record_success(&self, handle: &EgressHandle) {
        self.with_slots(|slots| {
            if let Some(slot) = slots.get(handle.index) {
                self.push_score(slot, SCORE_SUCCESS);
                self.with_machine(slot, |m| m.on_success(now_ms()));
            }
        });
    }

    /// 429 → 健康分 −2、Cooldown（尊重 Retry-After）。
    pub fn record_rate_limited(&self, handle: &EgressHandle, retry_after_secs: Option<u64>) {
        self.with_slots(|slots| {
            if let Some(slot) = slots.get(handle.index) {
                self.push_score(slot, SCORE_RATE_LIMIT);
                self.with_machine(slot, |m| m.on_rate_limited(now_ms(), retry_after_secs));
            }
        });
    }

    /// 418 ERR_BN_LIMIT → 健康分 −10、直接 Banned（换端重放由适配器执行）。
    pub fn record_banned(&self, handle: &EgressHandle, reason: &str) {
        self.with_slots(|slots| {
            if let Some(slot) = slots.get(handle.index) {
                self.push_score(slot, SCORE_BANNED);
                self.with_machine(slot, |m| m.on_banned(now_ms(), reason));
            }
        });
    }

    /// 半开探测成功。
    pub fn record_probe_success(&self, handle: &EgressHandle) {
        self.with_slots(|slots| {
            if let Some(slot) = slots.get(handle.index) {
                self.push_score(slot, SCORE_SUCCESS);
                self.with_machine(slot, |m| m.on_probe_success(now_ms()));
            }
        });
    }

    /// 半开探测失败。
    pub fn record_probe_failure(&self, handle: &EgressHandle, reason: &str) {
        self.with_slots(|slots| {
            if let Some(slot) = slots.get(handle.index) {
                self.push_score(slot, SCORE_BANNED);
                self.with_machine(slot, |m| m.on_probe_failure(now_ms(), reason));
            }
        });
    }

    /// 运维：手动封禁（P1-6 后门之一）。
    pub fn admin_ban(&self, index: usize) -> Result<(), String> {
        self.with_slots(|slots| {
            let slot = slots
                .get(index)
                .ok_or_else(|| format!("egress #{index} 不存在"))?;
            self.with_machine(slot, |m| m.admin_ban(now_ms()));
            Ok(())
        })
    }

    /// 运维：手动解封（banned 不再只靠重启恢复）。
    pub fn admin_unban(&self, index: usize) -> Result<(), String> {
        self.with_slots(|slots| {
            let slot = slots
                .get(index)
                .ok_or_else(|| format!("egress #{index} 不存在"))?;
            self.with_machine(slot, |m| m.admin_unban(now_ms()));
            Ok(())
        })
    }

    /// 按脱敏标签/URL 查找出口序号（管理 API 用：`direct` / `proxy#N` / URL）。
    pub fn find(&self, key: &str) -> Option<usize> {
        self.with_slots(|slots| {
            slots.iter().enumerate().find_map(|(i, s)| {
                let label = scope_of(i, &s.proxy_url).to_string();
                let url = s.proxy_url.as_deref().unwrap_or("direct");
                if key == label || key == url || key == i.to_string() {
                    Some(i)
                } else {
                    None
                }
            })
        })
    }

    /// 全部出口不可用时的分类提示：(封禁出口的 scope, 最短冷却剩余秒数)。
    pub fn unavailable_hint(&self) -> (Option<EgressScope>, Option<u64>) {
        let now = now_ms();
        self.with_slots(|slots| {
            let mut banned: Option<EgressScope> = None;
            let mut cooldown: Option<u64> = None;
            for (i, s) in slots.iter().enumerate() {
                let (state, remaining) = self.with_machine(s, |m| {
                    let st = m.status(now);
                    (st.state, m.remaining_secs(now))
                });
                match state {
                    EgressState::Banned if banned.is_none() => {
                        banned = Some(scope_of(i, &s.proxy_url));
                    }
                    EgressState::Cooldown => {
                        cooldown = Some(cooldown.map_or(remaining, |c: u64| c.min(remaining)));
                    }
                    _ => {}
                }
            }
            (banned, cooldown)
        })
    }

    /// WebUI/健康检查快照（含脱敏代理与滑动健康分）。
    pub fn snapshot(&self) -> Vec<EgressSnapshot> {
        let now = now_ms();
        self.with_slots(|slots| {
            slots
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    let (state, since, reason) = self.with_machine(s, |m| {
                        let st = m.status(now);
                        (st.state.as_str().to_string(), st.since_ms, st.reason)
                    });
                    EgressSnapshot {
                        index: i,
                        label: scope_of(i, &s.proxy_url).to_string(),
                        state,
                        since_ms: since,
                        reason,
                        score: score_in(slots, i) as i64,
                        inflight: s.inflight.load(Ordering::SeqCst),
                        proxy: s
                            .proxy_url
                            .as_deref()
                            .map(sanitize_proxy_url)
                            .filter(|u| u != "direct"),
                    }
                })
                .collect()
        })
    }

    /// 运维：运行时更换代理列表（管理面「增删代理」直达传输层，非死配置）。
    ///
    /// - 仍保留的出口**原样保留**槽位（状态机与健康分不丢，封禁不因改池复活）；
    /// - 新增出口新建槽位；被移除的出口直接丢弃；列表清空 → 恢复单一直连出口；
    /// - 有在途请求时拒绝（句柄按下标归还，重建会错配）；返回错误串供管理面直出。
    pub fn reconfigure(&self, proxies: &[String]) -> Result<(), String> {
        for p in proxies {
            validate_proxy(p).map_err(|e| e.to_string())?;
        }
        let mut slots = self.slots.write().unwrap_or_else(|e| e.into_inner());
        let inflight: usize = slots
            .iter()
            .map(|s| s.inflight.load(Ordering::SeqCst))
            .sum();
        if inflight > 0 {
            return Err("有在途请求，代理列表暂不可变更，请稍后重试".to_string());
        }
        let mut old: Vec<Slot> = std::mem::take(&mut *slots);
        let mut next: Vec<Slot> = Vec::with_capacity(proxies.len().max(1));
        for p in proxies {
            match old
                .iter()
                .position(|s| s.proxy_url.as_deref() == Some(p.as_str()))
            {
                Some(pos) => next.push(old.remove(pos)),
                None => next.push(new_slot(Some(p.clone()))),
            }
        }
        if next.is_empty() {
            next.push(new_slot(None));
        }
        *slots = next;
        drop(slots);
        if let Ok(mut st) = self.sticky.lock() {
            st.clear();
        }
        Ok(())
    }

    /// 测试辅助：滑动健康分。
    #[cfg(test)]
    fn score_of(&self, index: usize) -> i32 {
        self.with_slots(|slots| score_in(slots, index))
    }

    fn push_score(&self, slot: &Slot, delta: i32) {
        if let Ok(mut w) = slot.score.lock() {
            w.push_back(delta);
            while w.len() > SCORE_WINDOW {
                w.pop_front();
            }
        }
    }

    fn with_machine<T>(&self, slot: &Slot, f: impl FnOnce(&mut EgressMachine) -> T) -> T {
        match slot.machine.lock() {
            Ok(mut m) => f(&mut m),
            Err(poisoned) => f(&mut poisoned.into_inner()),
        }
    }
}

fn scope_of(index: usize, proxy_url: &Option<String>) -> EgressScope {
    match proxy_url {
        Some(_) => EgressScope::Proxy(index),
        None => EgressScope::Direct,
    }
}

/// 滑动窗口健康分（调用方已持 slots 借用，避免嵌套读锁）。
fn score_in(slots: &[Slot], index: usize) -> i32 {
    slots
        .get(index)
        .and_then(|s| s.score.lock().ok())
        .map(|w| w.iter().sum())
        .unwrap_or(0)
}

#[cfg(test)]
impl EgressPool {
    /// 测试辅助：当前句柄的 inflight。
    fn inflight_of(&self, h: &EgressHandle) -> usize {
        self.with_slots(|slots| {
            slots
                .get(h.index)
                .map(|s| s.inflight.load(Ordering::SeqCst))
                .unwrap_or(0)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn urls(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_merges_dedupes_and_validates() {
        let merged = parse_proxy_config(
            Some("http://a:1, socks5://b:1080"),
            Some("http://a:1,https://c:443"),
        )
        .unwrap();
        assert_eq!(
            merged,
            vec![
                "http://a:1".to_string(),
                "socks5://b:1080".to_string(),
                "https://c:443".to_string()
            ]
        );
        assert!(parse_proxy_config(Some("ftp://x"), None).is_err());
        assert!(parse_proxy_config(Some("no-scheme"), None).is_err());
        assert_eq!(
            parse_proxy_config(None, None).unwrap(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn empty_config_is_direct_single_egress() {
        let pool = EgressPool::new(&[]).unwrap();
        assert_eq!(pool.len(), 1);
        let h = pool.acquire(None).unwrap();
        assert_eq!(h.scope, EgressScope::Direct);
        assert_eq!(h.proxy_url, None);
        pool.release(&h);
        assert_eq!(pool.inflight_of(&h), 0);
    }

    #[test]
    fn configured_proxies_only_no_direct_leak() {
        let pool = EgressPool::new(&urls(&["http://a:1", "http://b:1"])).unwrap();
        assert_eq!(pool.len(), 2);
        for _ in 0..4 {
            let h = pool.acquire(None).unwrap();
            assert!(h.proxy_url.is_some(), "配置代理后不得回落直连");
            pool.release(&h);
        }
    }

    #[test]
    fn session_stickiness_keeps_same_egress() {
        let pool = EgressPool::new(&urls(&["http://a:1", "http://b:1", "http://c:1"])).unwrap();
        let h1 = pool.acquire(Some("sess-x")).unwrap();
        pool.record_success(&h1);
        pool.release(&h1);
        for _ in 0..3 {
            let h = pool.acquire(Some("sess-x")).unwrap();
            assert_eq!(h.index, h1.index, "同会话必须粘同一条出口");
            pool.release(&h);
        }
    }

    /// P0-3 池级断言：418 把当前出口打入 Banned 后，下一次分发必须换到别的健康出口。
    #[test]
    fn banned_egress_is_switched_away() {
        let pool = EgressPool::new(&urls(&["http://a:1", "http://b:1"])).unwrap();
        let first = pool.acquire(Some("s")).unwrap();
        pool.record_banned(&first, "418 ERR_BN_LIMIT");
        pool.release(&first);
        assert_eq!(pool.banned_count(), 1);
        let next = pool.acquire(Some("s")).expect("必须换到另一条健康出口");
        assert_ne!(next.index, first.index);
        pool.release(&next);
    }

    #[test]
    fn all_unavailable_returns_none_then_unban_recovers() {
        let pool = EgressPool::new(&urls(&["http://a:1"])).unwrap();
        let h = pool.acquire(None).unwrap();
        pool.record_banned(&h, "418 ERR_BN_LIMIT");
        pool.release(&h);
        assert!(pool.acquire(None).is_none(), "Banned 不参与分发");
        pool.admin_unban(0).unwrap();
        assert!(pool.acquire(None).is_some(), "手动解封恢复（P1-6）");
    }

    #[test]
    fn per_egress_concurrency_limit_two() {
        let pool = EgressPool::new(&urls(&["http://a:1"])).unwrap();
        let h1 = pool.acquire(None).unwrap();
        let h2 = pool.acquire(None).unwrap();
        assert!(pool.acquire(None).is_none(), "超过每出口并发上限 2");
        pool.release(&h1);
        let h3 = pool.acquire(None).expect("释放后可再取");
        pool.release(&h2);
        pool.release(&h3);
    }

    #[test]
    fn snapshot_masks_credentials() {
        let pool = EgressPool::new(&urls(&["http://user:pass@host:8080"])).unwrap();
        let snap = pool.snapshot();
        assert_eq!(snap[0].label, "proxy#0");
        let proxy = snap[0].proxy.clone().unwrap();
        assert!(!proxy.contains("pass"), "快照禁止携带凭据：{proxy}");
        assert_eq!(proxy, "http://host:8080");
        assert_eq!(snap[0].state, "Healthy");
    }

    #[test]
    fn score_window_effects_ordering() {
        let pool = EgressPool::new(&urls(&["http://a:1", "http://b:1"])).unwrap();
        // a 连续两次 429（−2×2 = −4），b 成功（+1）→ 分发偏向 b
        let a = pool.acquire(None).unwrap();
        assert_eq!(a.index, 0);
        pool.record_rate_limited(&a, Some(1));
        pool.release(&a);
        let a2 = pool.acquire(None).unwrap();
        if a2.index == 0 {
            pool.record_rate_limited(&a2, Some(1));
            pool.release(&a2);
        } else {
            pool.release(&a2);
        }
        let b = pool.acquire(None).unwrap();
        assert_eq!(b.index, 1, "健康分更低的出口应被降权");
        pool.record_success(&b);
        pool.release(&b);
        assert!(pool.score_of(0) < pool.score_of(1));
    }

    #[test]
    fn proxy_urls_returns_raw_management_list() {
        let pool = EgressPool::new(&urls(&["http://user:pass@a:1"])).unwrap();
        assert_eq!(
            pool.proxy_urls(),
            vec!["http://user:pass@a:1".to_string()],
            "管理面拿原始列表（回显给前端时才脱敏）"
        );
        let direct = EgressPool::new(&[]).unwrap();
        assert!(direct.proxy_urls().is_empty(), "直连出口不进代理列表");
    }

    #[test]
    fn reconfigure_live_add_remove_and_validation() {
        let pool = EgressPool::new(&[]).unwrap();
        assert!(pool.proxy_urls().is_empty());
        // 新增（单直连 → 单代理）
        pool.reconfigure(&["http://a:1".into()]).unwrap();
        assert_eq!(pool.proxy_urls(), vec!["http://a:1".to_string()]);
        assert_eq!(pool.snapshot().len(), 1);
        // 非法 URL 拒绝且不改原列表
        let err = pool.reconfigure(&["ftp://x".into()]).unwrap_err();
        assert!(err.contains("scheme") || err.contains("不支持"), "{err}");
        assert_eq!(pool.proxy_urls(), vec!["http://a:1".to_string()]);
        // 清空 → 恢复单一直连
        pool.reconfigure(&[]).unwrap();
        assert!(pool.proxy_urls().is_empty());
        assert_eq!(pool.snapshot().len(), 1);
        assert_eq!(pool.snapshot()[0].label, "direct");
    }

    #[test]
    fn reconfigure_rejected_while_inflight() {
        let pool = EgressPool::new(&urls(&["http://a:1"])).unwrap();
        let h = pool.acquire(None).expect("句柄");
        let err = pool.reconfigure(&["http://b:2".into()]).unwrap_err();
        assert!(err.contains("在途"), "在途必须拒绝：{err}");
        assert_eq!(pool.proxy_urls(), vec!["http://a:1".to_string()]);
        pool.release(&h);
        pool.reconfigure(&["http://b:2".into()])
            .expect("释放后可改");
        assert_eq!(pool.proxy_urls(), vec!["http://b:2".to_string()]);
    }

    #[test]
    fn reconfigure_preserves_slot_state() {
        let pool = EgressPool::new(&urls(&["http://a:1", "http://b:2"])).unwrap();
        pool.admin_ban(0).expect("封禁 a");
        pool.reconfigure(&[
            "http://b:2".into(),
            "http://a:1".into(),
            "http://c:3".into(),
        ])
        .expect("改池");
        let snap = pool.snapshot();
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].proxy.as_deref(), Some("http://b:2"));
        assert_eq!(snap[1].proxy.as_deref(), Some("http://a:1"));
        assert_eq!(
            snap[1].state, "Banned",
            "保留槽位：封禁状态与健康分不因改池丢失/复活"
        );
        assert_eq!(snap[2].state, "Healthy", "新增出口从健康态起步");
    }
}
