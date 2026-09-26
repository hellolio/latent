//! provider 级自动重试(04 文档 §2.5 auto retry;pi 的 retryAssistantCall 由
//! harness/coding-agent 层经 streamFn 注入,低层循环无内建重试 —— 不变量 I2)。
//!
//! 装饰 `Provider`:内部消费整段流,若终态为可重试错误则按指数退避重试;
//! 只把**最后一次成功尝试**的事件回放给循环(UI 不看到失败尝试的增量)。
//! aborted 终态、不可重试错误(配额/账单)立即放行(rpi-ai retry 语义)。

use std::future::Future;
use std::sync::Arc;

use async_trait::async_trait;
use rpi_ai::{
    is_retryable_assistant_error, retry_delay_ms, AssistantMessage, AssistantMessageEvent,
    AssistantMessageEventStream, Model, Provider, RetryPolicy, StreamOptions, TranscriptContext,
};

/// 重试过程回调:session 用它发 `auto_retry_start/end` 事件。
pub trait RetryHooks: Send + Sync {
    /// 每次重试的退避睡眠前(1-indexed attempt)。
    fn on_retry_scheduled(&self, _attempt: u32, _max_attempts: u32, _delay_ms: u64, _error: &str) {}
    /// 循环结束时恰好一次;success 表示后续调用正常完成。
    fn on_retry_finished(&self, _success: bool, _attempt: u32, _final_error: Option<&str>) {}
}

/// 工厂:带自动重试的装饰 provider(不联网语义与 inner 一致)。
pub fn create_retrying_provider(
    inner: Arc<dyn Provider>,
    policy: RetryPolicy,
    hooks: Option<Arc<dyn RetryHooks>>,
) -> Arc<dyn Provider> {
    Arc::new(RetryingProvider { inner, policy, hooks })
}

pub struct RetryingProvider {
    inner: Arc<dyn Provider>,
    policy: RetryPolicy,
    hooks: Option<Arc<dyn RetryHooks>>,
}

impl RetryingProvider {
    /// 消费一次完整尝试;终态事件已缓冲(含全部流事件),成功时原样回放。
    fn consume_attempt(
        &self,
        model: &Model,
        ctx: TranscriptContext,
        opts: &StreamOptions,
    ) -> impl Future<Output = Vec<AssistantMessageEvent>> + Send + '_ {
        let inner = self.inner.clone();
        let model = model.clone();
        let opts = opts.clone();
        async move {
            let stream = inner.stream(&model, ctx, opts).await;
            let mut stream = std::pin::pin!(stream);
            let mut events: Vec<AssistantMessageEvent> = Vec::new();
            while let Some(event) = futures::StreamExt::next(&mut stream).await {
                let terminal =
                    matches!(event, AssistantMessageEvent::Done(_) | AssistantMessageEvent::Error(_));
                events.push(event);
                if terminal {
                    break;
                }
            }
            events
        }
    }
}

#[async_trait]
impl Provider for RetryingProvider {
    async fn stream(
        &self,
        model: &Model,
        ctx: TranscriptContext,
        opts: StreamOptions,
    ) -> AssistantMessageEventStream {
        let max_attempts = if self.policy.enabled { self.policy.max_retries } else { 0 };
        let mut attempt: u32 = 0;
        let mut last_retry: Option<(u32, String)> = None;
        let replay: Vec<AssistantMessageEvent>;

        loop {
            let events = self.consume_attempt(model, clone_context(&ctx), &opts).await;
            let terminal_error: Option<AssistantMessage> = events.iter().find_map(|event| match event {
                AssistantMessageEvent::Error(message) => Some((**message).clone()),
                _ => None,
            });

            let retryable = terminal_error
                .as_ref()
                .map(is_retryable_assistant_error)
                .unwrap_or(false);
            if !retryable || attempt >= max_attempts {
                if let Some((attempt, _)) = last_retry {
                    let final_error = terminal_error
                        .as_ref()
                        .and_then(|m| m.error_message.clone());
                    // success = 最终尝试不是 error 终态(rpi-ai retry_assistant_call 同语义)
                    let success = final_error.is_none();
                    if let Some(hooks) = &self.hooks {
                        hooks.on_retry_finished(success, attempt, final_error.as_deref());
                    }
                }
                replay = events;
                break;
            }

            attempt += 1;
            let error_message = terminal_error
                .as_ref()
                .and_then(|m| m.error_message.clone())
                .unwrap_or_else(|| "unknown error".into());
            let delay_ms = retry_delay_ms(&self.policy, attempt);
            if let Some(hooks) = &self.hooks {
                hooks.on_retry_scheduled(attempt, max_attempts, delay_ms, &error_message);
            }
            // 退避中 abort → 归一化为 aborted 终态(rpi-ai retry 语义)
            if let Some(cancel) = &opts.cancel {
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_millis(delay_ms)) => {}
                    _ = cancel.cancelled() => {
                        replay = vec![AssistantMessageEvent::Error(Box::new(
                            AssistantMessage::error(model, "aborted", true),
                        ))];
                        break;
                    }
                }
            } else {
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            }
            last_retry = Some((attempt, error_message));
        }

        Box::pin(async_stream::stream! {
            for event in replay {
                yield event;
            }
        })
    }
}

fn clone_context(ctx: &TranscriptContext) -> TranscriptContext {
    TranscriptContext { messages: ctx.messages.clone() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_ai::{ContentBlock, MockProvider, ScriptedProvider, ScriptedTurn, StopReason};
    use std::sync::atomic::{AtomicU32, Ordering};

    fn model() -> Model {
        Model::minimal("mock-1", "mock", "mock")
    }

    async fn terminal_of(stream: &mut AssistantMessageEventStream) -> AssistantMessageEvent {
        use futures::StreamExt;
        let mut last = None;
        while let Some(event) = stream.next().await {
            last = Some(event);
        }
        last.expect("stream must have terminal event")
    }

    #[tokio::test]
    async fn retries_transient_error_and_replays_final_attempt() {
        let m = model();
        // 第 1 次请求失败(429),第 2 次成功
        let scripted = ScriptedProvider::new(
            &m,
            vec![
                ScriptedTurn::error(&m, "HTTP 429 too many requests"),
                ScriptedTurn::text(&m, "recovered"),
            ],
        );
        let scheduled = Arc::new(AtomicU32::new(0));
        struct Hooks(Arc<AtomicU32>);
        impl RetryHooks for Hooks {
            fn on_retry_scheduled(&self, attempt: u32, _max: u32, _delay_ms: u64, _error: &str) {
                self.0.store(attempt, Ordering::SeqCst);
            }
        }
        let provider = create_retrying_provider(
            Arc::new(scripted),
            RetryPolicy { base_delay_ms: 1, ..Default::default() },
            Some(Arc::new(Hooks(scheduled.clone()))),
        );
        let mut stream = provider
            .stream(&m, TranscriptContext { messages: vec![] }, StreamOptions::default())
            .await;
        let terminal = terminal_of(&mut stream).await;
        match terminal {
            AssistantMessageEvent::Done(message) => {
                assert_eq!(message.text_content(), "recovered");
            }
            other => panic!("expected done, got {other:?}"),
        }
        assert_eq!(scheduled.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn quota_errors_are_not_retried() {
        let m = model();
        let scripted = ScriptedProvider::new(
            &m,
            vec![ScriptedTurn::error(&m, "insufficient_quota: out of budget")],
        );
        let provider = create_retrying_provider(
            Arc::new(scripted),
            RetryPolicy { base_delay_ms: 1, ..Default::default() },
            None,
        );
        let mut stream = provider
            .stream(&m, TranscriptContext { messages: vec![] }, StreamOptions::default())
            .await;
        match terminal_of(&mut stream).await {
            AssistantMessageEvent::Error(message) => {
                assert_eq!(message.stop_reason, StopReason::Error);
            }
            other => panic!("expected error terminal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn plain_mock_passes_through() {
        let m = model();
        let provider = create_retrying_provider(
            Arc::new(MockProvider::new("hi")),
            RetryPolicy::default(),
            None,
        );
        let mut stream = provider
            .stream(&m, TranscriptContext { messages: vec![] }, StreamOptions::default())
            .await;
        match terminal_of(&mut stream).await {
            AssistantMessageEvent::Done(message) => {
                assert_eq!(message.text_content(), "hi");
                assert!(message.content.iter().any(|b| matches!(b, ContentBlock::Text { .. })));
            }
            other => panic!("expected done, got {other:?}"),
        }
    }

}
