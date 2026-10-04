//! 结构化请求日志环形缓冲（默认 500 条；不含上游响应全文与任何密钥）。
//!
//! 由 duckai-server 装配进 [`crate::ApiState`]，处理器在请求结束时写入；
//! WebUI Logs 页经 duckai-server 的 `AdminState::logs` 只读取。

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use duckai_types::LogEntry;

const DEFAULT_CAP: usize = 500;

/// 定长环形日志。
pub struct LogRing {
    cap: usize,
    inner: Mutex<VecDeque<LogEntry>>,
}

impl LogRing {
    pub fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            inner: Mutex::new(VecDeque::with_capacity(cap.min(DEFAULT_CAP))),
        }
    }

    /// 毒锁容忍：锁中毒时退化为持锁继续（只丢一致性、不丢可用性）。
    fn with<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut VecDeque<LogEntry>) -> R,
    {
        match self.inner.lock() {
            Ok(mut g) => f(&mut g),
            Err(p) => f(&mut p.into_inner()),
        }
    }

    pub fn push(&self, entry: LogEntry) {
        self.with(|q| {
            if q.len() >= self.cap {
                q.pop_front();
            }
            q.push_back(entry);
        });
    }

    /// 最近 n 条（新的在前）。
    pub fn recent(&self, n: usize) -> Vec<LogEntry> {
        self.with(|q| q.iter().rev().take(n.min(self.cap)).cloned().collect())
    }

    pub fn len(&self) -> usize {
        self.with(|q| q.len())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for LogRing {
    fn default() -> Self {
        Self::new(DEFAULT_CAP)
    }
}

/// epoch 毫秒。
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// epoch 秒。
pub fn now_secs() -> u64 {
    now_ms() / 1000
}
