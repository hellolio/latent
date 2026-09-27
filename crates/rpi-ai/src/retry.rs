//! provider 级重试(pi 的 utils/retry.ts,02 文档 §4.1)。
//!
//! 指数退避 `baseDelayMs * 2^(attempt-1)`,封顶 `maxAgentDelayMs`;
//! aborted 永不重试;退避中 abort → 归一化为 aborted 消息;
//! quota/billing/订阅限额不可重试。
//!
//! 另含 `RetryingProvider`(11 计划 T1):流式重试装饰器,SSE 帧级缓冲 ——
//! 提交点之前的帧(Start/*_start)先缓冲,首个内容 delta 到达时一次性放行
//! 缓冲并开始逐帧直通;提交点前遇到可重试失败则静默截断整轮重试(下游无
//! 重复 delta),提交点后失败按"失败编码进流"直通终止,不做中途重试
//! (policy §4:不许偏离)。重试/overflow 属 ai 层(02 文档),rpi-core 保留
//! 薄装配厂。

use std::sync::{Arc, LazyLock};

use regex::Regex;
use tokio_util::sync::CancellationToken;

use crate::provider::{AssistantMessageEventStream, Provider};
use crate::types::{
    AssistantMessage, AssistantMessageEvent, Model, StopReason, StreamOptions, TranscriptContext,
};

fn build_pattern(patterns: &[&str]) -> Regex {
    Regex::new(&format!("(?i)(?:{})", patterns.join("|")))
        .expect("static retry pattern must compile")
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
        "529",
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
        RetryPolicy {
            enabled: true,
            max_retries: 3,
            base_delay_ms: 1000,
            max_agent_delay_ms: None,
        }
    }
}

pub const DEFAULT_MAX_AGENT_RETRY_DELAY_MS: u64 = 60_000;

pub fn retry_delay_ms(policy: &RetryPolicy, attempt: u32) -> u64 {
    let delay = policy
        .base_delay_ms
        .saturating_mul(1u64 << attempt.saturating_sub(1).min(63));
    delay.min(
        policy
            .max_agent_delay_ms
            .unwrap_or(DEFAULT_MAX_AGENT_RETRY_DELAY_MS),
    )
}

/// 从错误文案解析服务端 Retry-After 标记(adapters 的 http_error_message 写入
/// "(retry-after: Ns)");优先于指数退避,尊重服务端退避节奏(如 429/529 限流)。
pub fn retry_after_ms_from_message(message: &str) -> Option<u64> {
    static PATTERN: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)retry-after:\s*(\d+)s").expect("static pattern"));
    let captures = PATTERN.captures(message)?;
    captures[1].parse::<u64>().ok().map(|s| s.saturating_mul(1000))
}

/// 失败的 assistant 消息是否像瞬时 provider/传输错误。
/// 这里不实现重试预算;overflow 由调用方先行单独处理(02 文档 §4.1)。
pub fn is_retryable_assistant_error(message: &AssistantMessage) -> bool {
    if message.stop_reason != StopReason::Error {
        return false;
    }
    let Some(error_message) = &message.error_message else {
        return false;
    };
    if NON_RETRYABLE_PROVIDER_LIMIT_ERROR.is_match(error_message) {
        return false;
    }
    RETRYABLE_PROVIDER_ERROR.is_match(error_message)
}

/// 重试回调(全部可选)。
pub trait RetryCallbacks: Send + Sync {
    /// 每次重试的退避睡眠前(1-indexed attempt)。
    fn on_retry_scheduled(
        &self,
        _attempt: u32,
        _max_attempts: u32,
        _delay_ms: u64,
        _error_message: &str,
    ) {
    }
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
    let max_attempts = policy
        .filter(|p| p.enabled)
        .map(|p| p.max_retries)
        .unwrap_or(0);
    let mut attempt: u32 = 0;
    let mut last_retry: Option<(u32, String)> = None;

    loop {
        let response = produce().await;

        if response.stop_reason == StopReason::Aborted {
            if let Some((attempt, _)) = last_retry {
                if let Some(cb) = callbacks {
                    cb.on_retry_finished(false, attempt, None)
                }
            }
            return response;
        }
        if response.stop_reason != StopReason::Error {
            if let Some((attempt, _)) = last_retry {
                if let Some(cb) = callbacks {
                    cb.on_retry_finished(true, attempt, None)
                }
            }
            return response;
        }
        if attempt >= max_attempts || !is_retryable_assistant_error(&response) {
            if let Some((attempt, _)) = last_retry {
                let error = response.error_message.clone();
                if let Some(cb) = callbacks {
                    cb.on_retry_finished(false, attempt, error.as_deref())
                }
            }
            return response;
        }

        attempt += 1;
        let error_message = response
            .error_message
            .clone()
            .unwrap_or_else(|| "Unknown error".into());
        let delay_ms = retry_after_ms_from_message(&error_message)
            .or_else(|| policy.map(|p| retry_delay_ms(p, attempt)))
            .unwrap_or(0);
        if let Some(cb) = callbacks {
            cb.on_retry_scheduled(attempt, max_attempts, delay_ms, &error_message);
        }
        last_retry = Some((attempt, error_message));

        if !sleep_with_cancel(delay_ms, cancel).await {
            let (attempt, error) = last_retry.expect("last_retry set above");
            if let Some(cb) = callbacks {
                cb.on_retry_finished(false, attempt, Some(error.as_str()))
            }
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

// ---------------------------------------------------------------------------
// RetryingProvider:流式重试装饰器(11 计划 T1,SSE 帧级缓冲)
// ---------------------------------------------------------------------------

/// 工厂:带自动重试的流式装饰 provider。未启用重试时零改动直通。
pub fn create_retrying_provider(
    inner: Arc<dyn Provider>,
    policy: RetryPolicy,
    callbacks: Option<Arc<dyn RetryCallbacks>>,
) -> Arc<dyn Provider> {
    Arc::new(RetryingProvider {
        inner,
        policy,
        callbacks,
    })
}

/// 流式重试装饰器:提交点(首个内容 delta)之前的帧先缓冲,可重试失败在
/// 提交点前发生 → 静默截断整轮、退避后重试(下游看不到失败尝试的任何帧);
/// 提交点后失败 → 按失败编码进流直通,不做中途重试(policy §4)。
struct RetryingProvider {
    inner: Arc<dyn Provider>,
    policy: RetryPolicy,
    callbacks: Option<Arc<dyn RetryCallbacks>>,
}

fn is_content_delta(event: &AssistantMessageEvent) -> bool {
    matches!(
        event,
        AssistantMessageEvent::TextDelta { .. }
            | AssistantMessageEvent::ThinkingDelta { .. }
            | AssistantMessageEvent::ToolCallDelta { .. }
    )
}

#[async_trait::async_trait]
impl Provider for RetryingProvider {
    async fn stream(
        &self,
        model: &Model,
        ctx: TranscriptContext,
        opts: StreamOptions,
    ) -> AssistantMessageEventStream {
        // 未启用重试:零改动直通,不引入任何缓冲
        if !self.policy.enabled || self.policy.max_retries == 0 {
            return self.inner.stream(model, ctx, opts).await;
        }
        let max_attempts = self.policy.max_retries;
        let inner = self.inner.clone();
        let model = model.clone();
        let callbacks = self.callbacks.clone();
        let policy = self.policy.clone();

        Box::pin(async_stream::stream! {
            let mut attempt: u32 = 0;
            // (attempt, error):最终以失败收尾时上报
            let mut last_retry: Option<(u32, String)> = None;
            let mut final_success = false;

            'attempts: loop {
                let mut buffer: Vec<AssistantMessageEvent> = Vec::new();
                let mut held_terminal: Option<AssistantMessageEvent> = None;
                let mut committed = false;
                let mut saw_terminal = false;

                let stream = inner.stream(&model, clone_context(&ctx), opts.clone()).await;
                let mut stream = std::pin::pin!(stream);
                while let Some(event) = futures::StreamExt::next(&mut stream).await {
                    let is_terminal =
                        matches!(event, AssistantMessageEvent::Done(_) | AssistantMessageEvent::Error(_));
                    if !committed {
                        if is_content_delta(&event) {
                            // 提交点:先按原顺序放行缓冲帧,紧随放行本 delta,此后逐帧直通
                            committed = true;
                            for buffered in std::mem::take(&mut buffer) {
                                yield buffered;
                            }
                            yield event;
                        } else if is_terminal {
                            held_terminal = Some(event);
                            break;
                        } else {
                            buffer.push(event);
                        }
                        continue;
                    }
                    // 提交点后逐帧直通(含终态)
                    final_success = is_terminal && matches!(event, AssistantMessageEvent::Done(_));
                    if is_terminal {
                        saw_terminal = true;
                        // 回调先于终态 yield:消费端收到终态即可能停止 poll
                        report_finished(&callbacks, &mut last_retry, final_success);
                        yield event;
                        break;
                    }
                    yield event;
                }

                if committed {
                    // 内容已流给下游:无论终态成败都直通过了,不做中途重试(policy §4);
                    // 流意外无终态时合成错误终态,保证下游流协议闭合(与未提交路径对称)
                    if !saw_terminal {
                        yield AssistantMessageEvent::Error(Box::new(AssistantMessage::error(
                            &model,
                            "stream ended without a terminal event",
                            false,
                        )));
                    }
                    break 'attempts;
                }

                // 未提交:流意外断掉时合成可分类的错误终态
                let held = held_terminal.unwrap_or_else(|| {
                    AssistantMessageEvent::Error(Box::new(AssistantMessage::error(
                        &model,
                        "stream ended without a terminal event",
                        false,
                    )))
                });
                let error_message: AssistantMessage = match &held {
                    AssistantMessageEvent::Error(message) => (**message).clone(),
                    AssistantMessageEvent::Done(_) => {
                        // Done 终态但无内容 delta(空回复):按原顺序放行缓冲帧 + 终态
                        // (终态必须 yield,否则下游按"流无终态"兜底成 error,P0 回归)
                        final_success = true;
                        report_finished(&callbacks, &mut last_retry, true);
                        for buffered in buffer {
                            yield buffered;
                        }
                        yield held;
                        break 'attempts;
                    }
                    _ => unreachable!("held_terminal 只可能是终态事件"),
                };

                let cancelled = opts.cancel.as_ref().map(|c| c.is_cancelled()).unwrap_or(false);
                let retryable = !cancelled
                    && attempt < max_attempts
                    && crate::retry::is_retryable_assistant_error(&error_message);
                if !retryable {
                    // 终态放行:缓冲帧(Start/*_start)+ 终态,顺序与原流一致
                    report_finished(&callbacks, &mut last_retry, false);
                    for buffered in buffer {
                        yield buffered;
                    }
                    yield held;
                    final_success = false;
                    break 'attempts;
                }

                attempt += 1;
                let error_text = error_message.error_message.clone().unwrap_or_default();
                let delay_ms = retry_after_ms_from_message(&error_text)
                    .unwrap_or_else(|| retry_delay_ms(&policy, attempt));
                if let Some(cb) = &callbacks {
                    cb.on_retry_scheduled(attempt, max_attempts, delay_ms, &error_text);
                }
                // 退避中 abort → 归一化为 aborted 终态(retry_assistant_call 同语义)
                let proceed = match &opts.cancel {
                    None => {
                        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                        true
                    }
                    Some(token) => {
                        tokio::select! {
                            _ = tokio::time::sleep(std::time::Duration::from_millis(delay_ms)) => true,
                            _ = token.cancelled() => false,
                        }
                    }
                };
                if !proceed {
                    for buffered in buffer {
                        yield buffered;
                    }
                    let aborted = AssistantMessage::error(&model, "aborted", true);
                    yield AssistantMessageEvent::Error(Box::new(aborted));
                    if let Some(cb) = &callbacks {
                        cb.on_retry_finished(false, attempt, Some(error_text.as_str()));
                    }
                    break 'attempts;
                }
                if let Some(cb) = &callbacks {
                    cb.on_retry_attempt_start();
                }
                last_retry = Some((attempt, error_text));
                // 缓冲帧随失败尝试一并丢弃:下游无重复 delta
            }

            // 提交点后流意外断掉(无终态)的兜底上报;其余路径已在终态 yield 前上报
            report_finished(&callbacks, &mut last_retry, final_success);
        })
    }
}

/// 收尾上报(恰一次):report 只发生在重试确实发生过的时候。
fn report_finished(
    callbacks: &Option<Arc<dyn RetryCallbacks>>,
    last_retry: &mut Option<(u32, String)>,
    success: bool,
) {
    if let Some((attempt, error)) = last_retry.take() {
        if let Some(cb) = callbacks {
            cb.on_retry_finished(success, attempt, (!success).then_some(error.as_str()));
        }
    }
}

fn clone_context(ctx: &TranscriptContext) -> TranscriptContext {
    TranscriptContext {
        messages: ctx.messages.clone(),
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
        let policy = RetryPolicy {
            base_delay_ms: 1000,
            max_agent_delay_ms: None,
            ..Default::default()
        };
        assert_eq!(retry_delay_ms(&policy, 1), 1000);
        assert_eq!(retry_delay_ms(&policy, 2), 2000);
        assert_eq!(retry_delay_ms(&policy, 3), 4000);
        let capped = RetryPolicy {
            base_delay_ms: 30_000,
            max_agent_delay_ms: Some(60_000),
            ..Default::default()
        };
        assert_eq!(retry_delay_ms(&capped, 5), 60_000);
    }

    #[test]
    fn retry_after_marker_is_parsed_from_error_message() {
        assert_eq!(
            retry_after_ms_from_message("HTTP 429: slow down (retry-after: 30s)"),
            Some(30_000)
        );
        assert_eq!(
            retry_after_ms_from_message("HTTP 529 (retry-after: 5s)"),
            Some(5_000)
        );
        assert_eq!(retry_after_ms_from_message("HTTP 503: no header"), None);
        // HTTP-date 形式不支持 → 回退指数退避
        assert_eq!(
            retry_after_ms_from_message("HTTP 429 (retry-after: Wed, 21 Oct 2015 07:28:00 GMT)"),
            None
        );
    }

    #[tokio::test]
    async fn retry_after_header_takes_precedence_over_backoff() {
        #[derive(Default)]
        struct Cb {
            delay_ms: std::sync::Mutex<Option<u64>>,
        }
        impl RetryCallbacks for Cb {
            fn on_retry_scheduled(&self, _a: u32, _m: u32, d: u64, _e: &str) {
                *self.delay_ms.lock().unwrap() = Some(d);
            }
        }
        let model = Model::minimal("m", "mock", "mock");
        let policy = RetryPolicy {
            base_delay_ms: 60_000,
            ..Default::default()
        };
        let mut calls = 0;
        let cb = Cb::default();
        let _ = retry_assistant_call(
            || {
                calls += 1;
                let calls = calls;
                let model = model.clone();
                async move {
                    if calls == 1 {
                        AssistantMessage::error(
                            &model,
                            "HTTP 429: limited (retry-after: 3s)",
                            false,
                        )
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
        assert_eq!(
            *cb.delay_ms.lock().unwrap(),
            Some(3_000),
            "应采用服务端 Retry-After 而非指数退避"
        );
    }

    #[test]
    fn classification_matches_transient_but_not_quota() {
        assert!(is_retryable_assistant_error(&error_message(
            "HTTP 429 Too Many Requests"
        )));
        // 529 overloaded(含空 body 时的 "HTTP 529")可重试
        assert!(is_retryable_assistant_error(&error_message("HTTP 529")));
        assert!(is_retryable_assistant_error(&error_message(
            "HTTP 529: overloaded_endpoint"
        )));
        assert!(is_retryable_assistant_error(&error_message(
            "Connection refused"
        )));
        assert!(is_retryable_assistant_error(&error_message(
            "Anthropic stream ended before message_stop"
        )));
        assert!(!is_retryable_assistant_error(&error_message(
            "insufficient_quota: billing cycle exhausted"
        )));
        assert!(!is_retryable_assistant_error(&error_message(
            "totally novel failure mode"
        )));
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
                self.scheduled
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let model = Model::minimal("m", "mock", "mock");
        let policy = RetryPolicy {
            base_delay_ms: 1,
            ..Default::default()
        };
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
        let policy = RetryPolicy {
            base_delay_ms: 60_000,
            max_retries: 3,
            ..Default::default()
        };
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
        let policy = RetryPolicy {
            base_delay_ms: 1,
            max_retries: 5,
            ..Default::default()
        };
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
