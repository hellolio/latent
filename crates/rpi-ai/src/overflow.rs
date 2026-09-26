//! 上下文溢出检测(pi 的 utils/overflow.ts,02 文档 §4.2)。
//!
//! 三类信号:错误文案模式(20+ provider)、z.ai 式静默溢出(usage > contextWindow)、
//! 小米式 length + 0 输出;以及可恢复 length 截断判定。上层据此决定"压缩后重试"。

use std::sync::LazyLock;

use regex::Regex;

use crate::types::{AssistantMessage, StopReason};

fn re(pattern: &str) -> Regex {
    Regex::new(&format!("(?i){pattern}")).expect("static overflow pattern must compile")
}

/// 各 provider 的"上下文超限"错误文案模式(overflow.ts OVERFLOW_PATTERNS)。
static OVERFLOW_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"prompt (?:is )?too long",
        r"request_too_large",
        r"input is too long for requested model",
        r"exceeds the context window",
        r"exceeds (?:the )?(?:model'?s )?maximum context length(?: of [\d,]+ tokens?|\s*\([\d,]+\))",
        r"input token count.*exceeds the maximum",
        r"maximum prompt length is \d+",
        r"reduce the length of the messages",
        r"maximum context length is \d+ tokens",
        r"exceeds (?:the )?maximum allowed input length of [\d,]+ tokens?",
        r"input \(\d+ tokens\) is longer than the model'?s context length \(\d+ tokens\)",
        r"exceeds the limit of \d+",
        r"exceeds the available context size",
        r"greater than the context length",
        r"context window exceeds limit",
        r"exceeded model token limit",
        r"too large for model with \d+ maximum context length",
        r"prompt has [\d,]+ tokens?, but the configured context size is [\d,]+ tokens?",
        r"model_context_window_exceeded",
        r"prompt too long; exceeded (?:max )?context length",
        r"range of input length should be",
        r"context[_ ]length[_ ]exceeded",
        r"too many tokens",
        r"token limit exceeded",
    ]
    .iter()
    .map(|p| re(p))
    .collect()
});

/// 非溢出错误(限流/服务端错误),即使命中溢出模式也排除
/// (如 Bedrock 的 "ThrottlingException: Too many tokens, …")。
static NON_OVERFLOW_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
        re(r"^(Throttling error|Service unavailable):"),
        re(r"rate limit"),
        re(r"too many requests"),
    ]
});

/// Cerebras 400/413 无 body 的溢出形态。
static CEREBRAS_BODYLESS_OVERFLOW: LazyLock<Regex> =
    LazyLock::new(|| re(r"^4(?:00|13)\s*(?:status code)?\s*\(no body\)"));

/// assistant 消息是否代表上下文溢出(02 文档 §4.2 isContextOverflow)。
///
/// `context_window` 用于静默溢出与 length+0 输出两类信号,传入模型窗口即可检测。
pub fn is_context_overflow(message: &AssistantMessage, context_window: Option<u64>) -> bool {
    // Case 1: 错误文案模式(先排除限流等非溢出错误)
    if message.stop_reason == StopReason::Error {
        if let Some(error_message) = &message.error_message {
            let is_non_overflow = NON_OVERFLOW_PATTERNS
                .iter()
                .any(|p| p.is_match(error_message));
            if !is_non_overflow {
                if OVERFLOW_PATTERNS.iter().any(|p| p.is_match(error_message)) {
                    return true;
                }
                if message.provider == "cerebras"
                    && CEREBRAS_BODYLESS_OVERFLOW.is_match(error_message)
                {
                    return true;
                }
            }
        }
    }

    // Case 2: 静默溢出(z.ai 式)——成功返回但 usage 超过窗口
    if let Some(window) = context_window {
        if message.stop_reason == StopReason::Stop {
            let input_tokens = message.usage.input + message.usage.cache_read;
            if input_tokens > window {
                return true;
            }
        }

        // Case 3: length + 0 输出(小米式)——服务端把超长输入截到窗口,没有生成空间
        if message.stop_reason == StopReason::Length && message.usage.output == 0 {
            let input_tokens = message.usage.input + message.usage.cache_read;
            if input_tokens as f64 >= window as f64 * 0.99 {
                return true;
            }
        }
    }

    false
}

/// length 停止但输出低于预期上限 → 可恢复截断,调用方可做一次有界的压缩重试。
/// `desired_max_output` 必须是未经上下文钳制的原始上限。
pub fn is_recoverable_length(message: &AssistantMessage, desired_max_output: u64) -> bool {
    message.stop_reason == StopReason::Length
        && desired_max_output > 0
        && message.usage.output < desired_max_output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Model, Usage};

    fn error_msg(provider: &str, text: &str) -> AssistantMessage {
        let mut m = AssistantMessage::error(&Model::minimal("m", "mock", provider), text, false);
        m.provider = provider.into();
        m
    }

    fn message_with(stop: StopReason, usage: Usage) -> AssistantMessage {
        let mut m = AssistantMessage::pending(&Model::minimal("m", "mock", "mock"));
        m.stop_reason = stop;
        m.usage = usage;
        m
    }

    #[test]
    fn detects_provider_error_text() {
        assert!(is_context_overflow(
            &error_msg(
                "anthropic",
                "prompt is too long: 213462 tokens > 200000 maximum"
            ),
            None
        ));
        assert!(is_context_overflow(
            &error_msg(
                "openai",
                "Your input exceeds the context window of this model"
            ),
            None
        ));
        assert!(is_context_overflow(
            &error_msg(
                "groq",
                "Please reduce the length of the messages or completion"
            ),
            None
        ));
        assert!(is_context_overflow(
            &error_msg("zai", "{\"code\":\"1261\",\"message\":\"Prompt too long\"}"),
            None
        ));
        assert!(is_context_overflow(
            &error_msg("cerebras", "400 (no body)"),
            None
        ));
    }

    #[test]
    fn excludes_throttling_and_unrelated_errors() {
        // 命中 "too many tokens" 但被 ThrottlingException 前缀排除
        assert!(!is_context_overflow(
            &error_msg(
                "bedrock",
                "Throttling error: Too many tokens, please wait before trying again."
            ),
            None
        ));
        assert!(!is_context_overflow(
            &error_msg("openai", "something else went wrong"),
            None
        ));
    }

    #[test]
    fn detects_silent_and_length_overflows() {
        let usage = Usage {
            input: 130_000,
            cache_read: 0,
            ..Default::default()
        };
        assert!(is_context_overflow(
            &message_with(StopReason::Stop, usage),
            Some(128_000)
        ));
        // 窗口内不算溢出
        let small = Usage {
            input: 100,
            ..Default::default()
        };
        assert!(!is_context_overflow(
            &message_with(StopReason::Stop, small),
            Some(128_000)
        ));
        // length + 0 输出 + 输入填满窗口(≥99%)
        let full = Usage {
            input: 127_500,
            output: 0,
            ..Default::default()
        };
        assert!(is_context_overflow(
            &message_with(StopReason::Length, full),
            Some(128_000)
        ));
        // 有输出则不判溢出
        let with_output = Usage {
            input: 127_500,
            output: 10,
            ..Default::default()
        };
        assert!(!is_context_overflow(
            &message_with(StopReason::Length, with_output),
            Some(128_000)
        ));
        // 未传窗口时不做静默检测
        assert!(!is_context_overflow(
            &message_with(StopReason::Stop, usage),
            None
        ));
    }

    #[test]
    fn recoverable_length_requires_output_below_limit() {
        let mut m = message_with(
            StopReason::Length,
            Usage {
                output: 100,
                ..Default::default()
            },
        );
        assert!(is_recoverable_length(&m, 4096));
        assert!(!is_recoverable_length(&m, 0));
        m.usage.output = 4096;
        assert!(!is_recoverable_length(&m, 4096));
        m.stop_reason = StopReason::Stop;
        assert!(!is_recoverable_length(&m, 4096));
    }
}
