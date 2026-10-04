//! provider 级自动重试(04 文档 §2.5 auto retry)—— 薄装配厂。
//!
//! 重试逻辑本体在 latent-ai(02 文档把重试/overflow 划给 ai 层;11 计划 T1 注意
//! 事项 3):本 crate 只把 session 的 `RetryHooks`(AutoRetry 事件面)适配为
//! latent-ai 的 `RetryCallbacks` 并委托装配。流式语义(SSE 帧级缓冲、提交点前
//! 静默重试)见 `latent-ai::retry` 模块文档。

use std::sync::Arc;

use latent_ai::{Provider, RetryPolicy};

/// 重试过程回调:session 用它发 `auto_retry_start/end` 事件。
pub trait RetryHooks: Send + Sync {
    /// 每次重试的退避睡眠前(1-indexed attempt)。
    fn on_retry_scheduled(&self, _attempt: u32, _max_attempts: u32, _delay_ms: u64, _error: &str) {}
    /// 循环结束时恰好一次;success 表示后续调用正常完成。
    fn on_retry_finished(&self, _success: bool, _attempt: u32, _final_error: Option<&str>) {}
}

struct HooksAdapter(Arc<dyn RetryHooks>);

impl latent_ai::RetryCallbacks for HooksAdapter {
    fn on_retry_scheduled(&self, attempt: u32, max_attempts: u32, delay_ms: u64, error: &str) {
        self.0
            .on_retry_scheduled(attempt, max_attempts, delay_ms, error);
    }

    fn on_retry_finished(&self, success: bool, attempt: u32, final_error: Option<&str>) {
        self.0.on_retry_finished(success, attempt, final_error);
    }
}

/// 工厂:带自动重试的装饰 provider(不联网语义与 inner 一致)。
pub fn create_retrying_provider(
    inner: Arc<dyn Provider>,
    policy: RetryPolicy,
    hooks: Option<Arc<dyn RetryHooks>>,
) -> Arc<dyn Provider> {
    let callbacks: Option<Arc<dyn latent_ai::RetryCallbacks>> =
        hooks.map(|hooks| Arc::new(HooksAdapter(hooks)) as _);
    latent_ai::create_retrying_provider(inner, policy, callbacks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use latent_ai::{
        AssistantMessageEvent, ContentBlock, MockProvider, Model, ScriptedProvider, ScriptedTurn,
        StopReason, StreamOptions, TranscriptContext,
    };
    use std::sync::atomic::{AtomicU32, Ordering};

    fn model() -> Model {
        Model::minimal("mock-1", "mock", "mock")
    }

    async fn terminal_of(
        stream: &mut latent_ai::AssistantMessageEventStream,
    ) -> AssistantMessageEvent {
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
            RetryPolicy {
                base_delay_ms: 1,
                ..Default::default()
            },
            Some(Arc::new(Hooks(scheduled.clone()))),
        );
        let mut stream = provider
            .stream(
                &m,
                TranscriptContext { messages: vec![] },
                StreamOptions::default(),
            )
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
            RetryPolicy {
                base_delay_ms: 1,
                ..Default::default()
            },
            None,
        );
        let mut stream = provider
            .stream(
                &m,
                TranscriptContext { messages: vec![] },
                StreamOptions::default(),
            )
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
            .stream(
                &m,
                TranscriptContext { messages: vec![] },
                StreamOptions::default(),
            )
            .await;
        match terminal_of(&mut stream).await {
            AssistantMessageEvent::Done(message) => {
                assert_eq!(message.text_content(), "hi");
                assert!(message
                    .content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::Text { .. })));
            }
            other => panic!("expected done, got {other:?}"),
        }
    }
}
