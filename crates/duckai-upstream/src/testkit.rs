//! `testutil` feature：API 层契约测试用的脚本化上游替身。
//!
//! 每次 `chat()` 消费 [`ScriptedTurn`] 队列中的一条；队列耗尽后返回空流。
//! `seen` 记录收到的全部 `UpstreamRequest`，供 API 层断言扁平化/工具路由的入站形态。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use futures::stream;

use duckai_types::{ModelInfo, UpstreamError, UpstreamEvent};

use crate::{UpstreamClient, UpstreamRequest, UpstreamStream};

/// 一条剧本化对话（不可 Clone：`UpstreamError` 不实现 Clone）。
#[derive(Debug)]
pub enum ScriptedTurn {
    /// 按给定间隔逐条 yield 事件（真实流式节奏，由测试逐帧断言）。
    Chunked(Vec<UpstreamEvent>, Duration),
    /// 一次工具调用（Text 前置 + ToolCall + Done）。
    ToolCall {
        name: String,
        arguments: serde_json::Value,
    },
    /// 流建立即失败。
    Fail(UpstreamError),
    /// 永不结束的流（并发闸门测试用）；drop 时自动归还 inflight。
    Hang,
}

/// 保证未走完的流在 drop 时归还 inflight 计数。
struct InflightGuard(Arc<AtomicUsize>);

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// 契约测试替身。
pub struct MockUpstream {
    mode: &'static str,
    models: Vec<ModelInfo>,
    script: Mutex<VecDeque<ScriptedTurn>>,
    seen: Mutex<Vec<UpstreamRequest>>,
    /// 当前 inflight 流数（Hang 会一直持有）。
    inflight: Arc<AtomicUsize>,
}

impl MockUpstream {
    pub fn new() -> Self {
        Self {
            mode: "http",
            models: vec![
                ModelInfo::snapshot("mock-model", "duck.ai"),
                ModelInfo::snapshot("other-model", "duck.ai"),
            ],
            script: Mutex::new(VecDeque::new()),
            seen: Mutex::new(Vec::new()),
            inflight: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn with_mode(mut self, mode: &'static str) -> Self {
        self.mode = mode;
        self
    }

    pub fn with_models(mut self, models: Vec<ModelInfo>) -> Self {
        self.models = models;
        self
    }

    /// 压入剧本条目（可多次调用）。
    pub fn push(&self, turn: ScriptedTurn) {
        if let Ok(mut q) = self.script.lock() {
            q.push_back(turn);
        }
    }

    /// 断言辅助：收到过的全部请求。
    pub fn seen(&self) -> Vec<UpstreamRequest> {
        self.seen.lock().map(|s| s.clone()).unwrap_or_default()
    }

    pub fn inflight(&self) -> usize {
        self.inflight.load(Ordering::SeqCst)
    }
}

impl Default for MockUpstream {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl UpstreamClient for MockUpstream {
    fn mode(&self) -> &'static str {
        self.mode
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, UpstreamError> {
        Ok(self.models.clone())
    }

    async fn chat(&self, req: UpstreamRequest) -> Result<UpstreamStream, UpstreamError> {
        if let Ok(mut seen) = self.seen.lock() {
            seen.push(req);
        }
        let turn = self.script.lock().ok().and_then(|mut q| q.pop_front());
        let Some(turn) = turn else {
            return Ok(stream::empty().boxed());
        };
        match turn {
            ScriptedTurn::Chunked(events, gap) => {
                self.inflight.fetch_add(1, Ordering::SeqCst);
                let guard = InflightGuard(self.inflight.clone());
                let items: VecDeque<Result<UpstreamEvent, UpstreamError>> =
                    events.into_iter().map(Ok).collect();
                let stream =
                    stream::unfold((items, guard, gap), |(mut it, guard, gap)| async move {
                        match it.pop_front() {
                            Some(ev) => {
                                if gap > Duration::ZERO {
                                    tokio::time::sleep(gap).await;
                                }
                                Some((ev, (it, guard, gap)))
                            }
                            // state（含 guard）在此 drop → inflight 归还
                            None => None,
                        }
                    });
                Ok(stream.boxed())
            }
            ScriptedTurn::ToolCall { name, arguments } => Ok(stream::iter(vec![
                Ok(UpstreamEvent::TextDelta("调用工具中…".into())),
                Ok(UpstreamEvent::ToolCall {
                    id: format!("call_{name}"),
                    name,
                    arguments,
                }),
                Ok(UpstreamEvent::Done {
                    finish_reason: "tool_calls".into(),
                }),
            ])
            .boxed()),
            ScriptedTurn::Fail(err) => Err(err),
            ScriptedTurn::Hang => {
                self.inflight.fetch_add(1, Ordering::SeqCst);
                let guard = InflightGuard(self.inflight.clone());
                // 永不 yield；drop 本流时 guard 归还计数
                let stream = stream::once(async move {
                    let _keep = guard;
                    futures::future::pending::<()>().await;
                    unreachable!()
                });
                Ok(stream.boxed())
            }
        }
    }

    async fn probe(&self) -> Result<(), UpstreamError> {
        Ok(())
    }
}
