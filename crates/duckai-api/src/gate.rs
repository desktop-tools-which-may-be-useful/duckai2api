//! 全局并发闸门（ARCHITECTURE §6 / P1-7：`DUCKAI_MAX_CONCURRENCY`，默认 8）。
//!
//! 取不到令牌时不排队、不砸向上流：API 层直接 429 + `retry-after`。
//! 流式响应的令牌随请求体存活——[`GatePermit`] 被 move 进 SSE 流，
//! 流结束（含 drop）即归还。
//!
//! 实现说明：用原子计数而非 `tokio::Semaphore`，因为管理面的
//! `set_max_concurrency` 需要**运行时**调整上限（Semaphore 容量固定，
//! 换芯会让在途配额错乱）。语义等价：非阻塞获取、drop 归还。

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// 一次 chat 的并发配额；drop 恰好归还一次。
pub struct GatePermit {
    counter: Arc<AtomicUsize>,
}

impl Drop for GatePermit {
    fn drop(&mut self) {
        // fetch_sub：与并发的 set_max 无交互；计数不可能低于 0（获取方恒 +1 后才持有）。
        let _ = self.counter.fetch_sub(1, Ordering::SeqCst);
    }
}

/// 全局在途 chat 请求数上限。
pub struct ConcurrencyGate {
    inflight: Arc<AtomicUsize>,
    max: AtomicUsize,
}

impl ConcurrencyGate {
    pub fn new(max: usize) -> Self {
        Self {
            inflight: Arc::new(AtomicUsize::new(0)),
            max: AtomicUsize::new(max.max(1)),
        }
    }

    pub fn max(&self) -> usize {
        self.max.load(Ordering::SeqCst)
    }

    /// 运维：运行时调整上限（管理面「并发上限」保存即生效；已在途的请求
    /// 不受影响，超出新上限的部分随完成自然回落，不再接新）。
    pub fn set_max(&self, n: usize) {
        self.max.store(n.max(1), Ordering::SeqCst);
    }

    /// 非阻塞获取；拿不到 → `None`（调用方出 429 + retry-after）。
    pub fn try_acquire(&self) -> Option<GatePermit> {
        loop {
            let cur = self.inflight.load(Ordering::SeqCst);
            if cur >= self.max.load(Ordering::SeqCst) {
                return None;
            }
            if self
                .inflight
                .compare_exchange(cur, cur + 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return Some(GatePermit {
                    counter: self.inflight.clone(),
                });
            }
        }
    }

    /// 当前在途数（观测用）。
    pub fn inflight(&self) -> usize {
        self.inflight.load(Ordering::SeqCst)
    }
}

impl Default for ConcurrencyGate {
    fn default() -> Self {
        Self::new(8)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_max_takes_effect_immediately() {
        let gate = ConcurrencyGate::new(2);
        let p1 = gate.try_acquire().expect("第一个配额");
        let _p2 = gate.try_acquire().expect("第二个配额");
        assert!(gate.try_acquire().is_none(), "2/2 已满");
        gate.set_max(3);
        let p3 = gate.try_acquire().expect("调高上限后立即可取");
        gate.set_max(1);
        assert!(gate.try_acquire().is_none(), "调低后在途未回落前不放新");
        drop((p1, p3));
        assert!(gate.try_acquire().is_none(), "在途仍 > 新上限");
        assert_eq!(gate.inflight(), 1);
    }
}
