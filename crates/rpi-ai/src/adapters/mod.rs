//! API 协议适配器(02 文档 §1):每个 API 协议一个模块,统一输出
//! `AssistantMessageEvent` 流;失败编码进流,不抛异常。

pub mod anthropic;
pub mod openai_completions;

use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::types::{AssistantMessage, AssistantMessageEvent, CacheRetention, Model};

/// 上游字节块读取结果(与取消令牌统一竞速)。
pub(crate) enum ReadOutcome {
    Chunk(Vec<u8>),
    Ended,
    Aborted,
    Transport(String),
}

pub(crate) type ByteStream = std::pin::Pin<Box<dyn futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>;

/// 读一个上游字节块;cancel 取消时返回 Aborted(流随后以 aborted 终态收尾)。
pub(crate) async fn read_chunk(stream: &mut ByteStream, cancel: Option<&CancellationToken>) -> ReadOutcome {
    let next = async { stream.next().await };
    let item = match cancel {
        None => next.await,
        Some(token) => tokio::select! {
            _ = token.cancelled() => return ReadOutcome::Aborted,
            item = next => item,
        },
    };
    match item {
        Some(Ok(bytes)) => ReadOutcome::Chunk(bytes.to_vec()),
        Some(Err(err)) => ReadOutcome::Transport(err.to_string()), // 保留原始文案供重试分类
        None => ReadOutcome::Ended,
    }
}

/// 解析缓存保持期:显式优先,其次 PI_CACHE_RETENTION=long,默认 short(pi 语义)。
pub(crate) fn resolve_cache_retention(explicit: Option<CacheRetention>) -> CacheRetention {
    explicit.unwrap_or_else(|| {
        if std::env::var("PI_CACHE_RETENTION").map(|v| v == "long").unwrap_or(false) {
            CacheRetention::Long
        } else {
            CacheRetention::Short
        }
    })
}

/// 去掉 baseUrl 尾部斜杠。
pub(crate) fn trim_base_url(base_url: &str) -> &str {
    base_url.trim_end_matches('/')
}

/// HTTP 错误响应 → 统一错误文案 `{status}: {body}`。
pub(crate) fn http_error_message(status: reqwest::StatusCode, body: &str) -> String {
    let body = body.trim();
    if body.is_empty() {
        format!("HTTP {}", status.as_u16())
    } else {
        format!("HTTP {}: {}", status.as_u16(), body)
    }
}

/// 请求建立失败:直接以 error 终态收尾(02 文档流协议:请求建立失败可跳过 start)。
pub(crate) fn setup_error(model: &Model, message: String) -> AssistantMessageEvent {
    AssistantMessageEvent::Error(Box::new(AssistantMessage::error(model, message, false)))
}

/// 取消后的终态 aborted 消息。
pub(crate) fn aborted_error(model: &Model) -> AssistantMessageEvent {
    AssistantMessageEvent::Error(Box::new(AssistantMessage::error(model, "Request was aborted", true)))
}


/// 请求头装配:适配器默认头先入表,用户请求头同名覆盖(pi 语义;
/// reqwest 的 header() 是追加语义,直接链式会出现重复头)。
pub(crate) fn build_header_map(
    defaults: &[(&'static str, String)],
    extra: &std::collections::HashMap<String, String>,
) -> reqwest::header::HeaderMap {
    let mut map = reqwest::header::HeaderMap::new();
    for (name, value) in defaults {
        if let Ok(v) = reqwest::header::HeaderValue::from_str(value) {
            map.insert(reqwest::header::HeaderName::from_static(name), v);
        }
    }
    for (name, value) in extra {
        if let Ok(name) = reqwest::header::HeaderName::from_bytes(name.as_bytes()) {
            if let Ok(v) = reqwest::header::HeaderValue::from_str(value) {
                map.insert(name, v);
            }
        }
    }
    map
}

/// 重放请求时的 thinking 级别 → provider 侧取值映射(01 文档 thinkingLevelMap)。
pub(crate) fn map_thinking_level(model: &Model, level: crate::types::ThinkingLevel) -> String {
    if let Some(map) = &model.thinking_level_map {
        if let Some(Some(mapped)) = map.get(level.as_str()) {
            return mapped.clone();
        }
    }
    level.as_str().to_string()
}
