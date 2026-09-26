//! provider 级重试(pi 的 utils/retry.ts,02 文档 §4.1)。
//!
//! 指数退避 `baseDelayMs * 2^(attempt-1)`,封顶 `maxAgentDelayMs`;
//! aborted 永不重试;退避中 abort → 归一化为 aborted 消息;
//! quota/billing/订阅限额不可重试。

use std::sync::LazyLock;

use regex::Regex;
use tokio_util::sync::CancellationToken;

use crate::types::{AssistantMessage, StopReason};

fn build_pattern(patterns: &[&str]) -> Regex {
    Regex::new(&format!("(?i)(?:{})", patterns.join("|"))).expect("static retry pattern must compile")
}

/// 订阅/配额/账单类错误:不可重试(retry.ts NON_RETRYABLE_PROVIDER_LIMIT_ERROR_PATTERN)。
static NON_RETRYABLE_PROVIDER_LIMIT_ERROR: LazyLock<Regex> = LazyLock::new(|| {
    build_pattern(&[
        "GoUsageLimitError",
        "FreeUsageLimitError",
        "Monthly usage limit reached",
        "available balance",
        "insufficient_quota",
        "out of budget",
        "quota exceeded",
        "billing",
    ])
});

/// 瞬时故障分类(429/5xx/网络/流早断等,retry.ts RETRYABLE_PROVIDER_ERROR_PATTERN)。
static RETRYABLE_PROVIDER_ERROR: LazyLock<Regex> = LazyLock::new(|| {
    build_pattern(&[
        "overloaded",
        "currently experiencing high demand",
        "rate.?limit",
        "too many requests",
        "429",
        "500",
        "502",
        "503",
        "504",
        "520",
        "524",
        "service.?unavailable",
        "server.?error",
        "internal.?error",
        "provider.?returned.?error",
        "exceeded request buffer limit while retrying upstream",
        "network.?error",
        "connection.?error",
        "connection.?refused",
        "connection.?lost",
        "other side closed",
        "fetch failed",
        "getaddrinfo",
        "ENOTFOUND",
        "EAI_AGAIN",
        "upstream.?connect",
        "reset before headers",
        "socket hang up",
        "socket connection was closed",
        "timed? out",
        "timeout",
        "terminated",
        "websocket.?closed",
        "websocket.?error",
        "ended without",
        "stream ended before message_stop",
        "stream ended before a terminal response event",
        "http2 request did not get a response",
        "retry delay",
        "you can retry your request",
        "try your request again",
        "please retry your request",
        "ResourceExhausted",
    ])
});

/// 重试策略:有界次数 + 指数退避(harness 默认 enabled/maxRetries=3/baseDelayMs=1000)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryPolicy {
    pub enabled: bool,
    /// 最大重试次数(0=不重试;首次调用不计入)
    pub max_retries: u32,
    pub base_delay_ms: u64,
    /// 单次退避封顶,默认 60s
    pub max_agent_delay_ms: Option<u64>,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy { enabled: true, max_retries: 3, base_delay_ms: 1000, max_agent_delay_ms: None }
    }
}

pub const DEFAULT_MAX_AGENT_RETRY_DELAY_MS: u64 = 60_000;

pub fn retry_delay_ms(policy: &RetryPolicy, attempt: u32) -> u64 {
    let delay = policy.base_delay_ms.saturating_mul(1u64 << attempt.saturating_sub(1).min(63));
    delay.min(policy.max_agent_delay_ms.unwrap_or(DEFAULT_MAX_AGENT_RETRY_DELAY_MS))
}

/// 失败的 assistant 消息是否像瞬时 provider/传输错误。
/// 这里不实现重试预算;overflow 由调用方先行单独处理(02 文档 §4.1)。
pub fn is_retryable_assistant_error(message: &AssistantMessage) -> bool {
    if message.stop_reason != StopReason::Error {
        return false;
    }
    let Some(error_message) = &message.error_message else { return false };
    if NON_RETRYABLE_PROVIDER_LIMIT_ERROR.is_match(error_message) {
        return false;
    }
    RETRYABLE_PROVIDER_ERROR.is_match(error_message)
}

/// 重试回调(全部可选)。
pub trait RetryCallbacks: Send + Sync {
    /// 每次重试的退避睡眠前(1-indexed attempt)。
    fn on_retry_scheduled(&self, _attempt: u32, _max_attempts: u32, _delay_ms: u64, _error_message: &str) {}
    /// 退避睡眠后、重试调用开始前。
    fn on_retry_attempt_start(&self) {}
    /// 循环结束时恰好一次(success 表示后续调用正常完成)。
    fn on_retry_finished(&self, _success: bool, _attempt: u32, _final_error: Option<&str>) {}
}

/// 无操作回调。
pub struct NoopCallbacks;

impl RetryCallbacks for NoopCallbacks {}

async fn sleep_with_cancel(ms: u64, cancel: Option<&CancellationToken>) -> bool {
    match cancel {
        None => {
            tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
            true
        }
        Some(token) => {
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_millis(ms)) => true,
                _ = token.cancelled() => false,
            }
        }
    }
}

/// 带重试地运行一次"产出 assistant 消息"的调用(02 文档 §4.1 retryAssistantCall)。
///
/// - 成功立即返回;aborted 终态、永不重试;
/// - 不可重试错误(含 quota/billing)立即返回,确定性错误快速失败;
/// - 否则按指数退避重试至 max_retries;退避中 abort → 归一化为 aborted 消息。
pub async fn retry_assistant_call<F, Fut>(
    mut produce: F,
    policy: Option<&RetryPolicy>,
    cancel: Option<&CancellationToken>,
    callbacks: Option<&dyn RetryCallbacks>,
) -> AssistantMessage
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = AssistantMessage>,
{
    let max_attempts = policy.filter(|p| p.enabled).map(|p| p.max_retries).unwrap_or(0);
    let mut attempt: u32 = 0;
    let mut last_retry: Option<(u32, String)> = None;

    loop {
        let response = produce().await;

        if response.stop_reason == StopReason::Aborted {
            if let Some((attempt, _)) = last_retry {
                if let Some(cb) = callbacks { cb.on_retry_finished(false, attempt, None) }
            }
            return response;
        }
        if response.stop_reason != StopReason::Error {
            if let Some((attempt, _)) = last_retry {
                if let Some(cb) = callbacks { cb.on_retry_finished(true, attempt, None) }
            }
            return response;
        }
        if attempt >= max_attempts || !is_retryable_assistant_error(&response) {
            if let Some((attempt, _)) = last_retry {
                let error = response.error_message.clone();
                if let Some(cb) = callbacks { cb.on_retry_finished(false, attempt, error.as_deref()) }
            }
            return response;
        }

        attempt += 1;
        let error_message = response.error_message.clone().unwrap_or_else(|| "Unknown error".into());
        let delay_ms = policy.map(|p| retry_delay_ms(p, attempt)).unwrap_or(0);
        if let Some(cb) = callbacks {
            cb.on_retry_scheduled(attempt, max_attempts, delay_ms, &error_message);
        }
        last_retry = Some((attempt, error_message));

        if !sleep_with_cancel(delay_ms, cancel).await {
            let (attempt, error) = last_retry.expect("last_retry set above");
            if let Some(cb) = callbacks { cb.on_retry_finished(false, attempt, Some(error.as_str())) }
            let mut aborted = response.clone();
            aborted.stop_reason = StopReason::Aborted;
            aborted.error_message = None; // pi 剥离 errorMessage,避免污染 aborted 语义
            return aborted;
        }
        if let Some(cb) = callbacks {
            cb.on_retry_attempt_start();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Model;

    fn error_message(text: &str) -> AssistantMessage {
        let model = Model::minimal("m", "mock", "mock");
        AssistantMessage::error(&model, text, false)
    }

    #[test]
    fn delay_grows_exponentially_and_is_capped() {
        let policy = RetryPolicy { base_delay_ms: 1000, max_agent_delay_ms: None, ..Default::default() };
        assert_eq!(retry_delay_ms(&policy, 1), 1000);
        assert_eq!(retry_delay_ms(&policy, 2), 2000);
        assert_eq!(retry_delay_ms(&policy, 3), 4000);
        let capped = RetryPolicy { base_delay_ms: 30_000, max_agent_delay_ms: Some(60_000), ..Default::default() };
        assert_eq!(retry_delay_ms(&capped, 5), 60_000);
    }

    #[test]
    fn classification_matches_transient_but_not_quota() {
        assert!(is_retryable_assistant_error(&error_message("HTTP 429 Too Many Requests")));
        assert!(is_retryable_assistant_error(&error_message("Connection refused")));
        assert!(is_retryable_assistant_error(&error_message("Anthropic stream ended before message_stop")));
        assert!(!is_retryable_assistant_error(&error_message("insufficient_quota: billing cycle exhausted")));
        assert!(!is_retryable_assistant_error(&error_message("totally novel failure mode")));
        // 非 error 终态不重试
        let ok = AssistantMessage::pending(&Model::minimal("m", "mock", "mock"));
        assert!(!is_retryable_assistant_error(&ok));
    }

    #[tokio::test]
    async fn retries_transient_errors_then_succeeds() {
        #[derive(Default)]
        struct Cb {
            scheduled: std::sync::atomic::AtomicUsize,
        }
        impl RetryCallbacks for Cb {
            fn on_retry_scheduled(&self, _a: u32, _m: u32, _d: u64, _e: &str) {
                self.scheduled.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let model = Model::minimal("m", "mock", "mock");
        let policy = RetryPolicy { base_delay_ms: 1, ..Default::default() };
        let mut calls = 0;
        let cb = Cb::default();
        let result = retry_assistant_call(
            || {
                calls += 1;
                let calls = calls;
                let model = model.clone();
                async move {
                    if calls < 3 {
                        AssistantMessage::error(&model, "HTTP 503 Service Unavailable", false)
                    } else {
                        let mut m = AssistantMessage::pending(&model);
                        m.stop_reason = StopReason::Stop;
                        m
                    }
                }
            },
            Some(&policy),
            None,
            Some(&cb),
        )
        .await;
        assert_eq!(result.stop_reason, StopReason::Stop);
        assert_eq!(cb.scheduled.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn abort_during_backoff_normalizes_to_aborted() {
        let model = Model::minimal("m", "mock", "mock");
        let policy = RetryPolicy { base_delay_ms: 60_000, max_retries: 3, ..Default::default() };
        let cancel = CancellationToken::new();
        let producer_cancel = cancel.clone();
        let result = retry_assistant_call(
            || {
                let cancel = producer_cancel.clone();
                let model = model.clone();
                async move {
                    // 触发退避后立刻取消
                    cancel.cancel();
                    AssistantMessage::error(&model, "HTTP 503", false)
                }
            },
            Some(&policy),
            Some(&cancel),
            None,
        )
        .await;
        assert_eq!(result.stop_reason, StopReason::Aborted);
    }

    #[tokio::test]
    async fn aborted_response_is_never_retried() {
        let model = Model::minimal("m", "mock", "mock");
        let policy = RetryPolicy { base_delay_ms: 1, max_retries: 5, ..Default::default() };
        let mut calls = 0;
        let result = retry_assistant_call(
            || {
                calls += 1;
                let model = model.clone();
                async move { AssistantMessage::error(&model, "HTTP 503", true) }
            },
            Some(&policy),
            None,
            None,
        )
        .await;
        assert_eq!(result.stop_reason, StopReason::Aborted);
        assert_eq!(calls, 1);
    }
}
