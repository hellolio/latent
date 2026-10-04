//! `anthropic-messages` 适配器(02 文档 §1;pi api/anthropic-messages.ts 的 Rust 端口)。
//!
//! 与 pi 的差异(有意裁剪,详见 docs/02 踩坑记录):不走 Anthropic SDK,直接
//! POST `{baseUrl}/v1/messages` + SSE;OAuth/Claude Code 伪装、server-side fallback、
//! managed-effort system 消息与 beta 头协商随 M6 按需补齐。

use std::collections::HashMap;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::adapters::{
    aborted_error, effective_max_tokens, error_with_partial, http_error_message,
    map_thinking_level, observe_payload, observe_provider_event, parse_retry_after_header,
    observe_response, read_chunk, resolve_cache_retention, setup_error, trim_base_url, ByteStream,
    ReadOutcome,
};
use crate::json_parse::{parse_json_with_repair, parse_streaming_json};
use crate::provider::{AssistantMessageEventStream, Provider};
use crate::sse::{LineDecoder, SseDecoder};
use crate::transcript::{
    get_current_tools, get_initial_system_message, get_system_message_text,
    render_system_message_update, resolve_transcript,
};
use crate::types::{
    AssistantMessage, AssistantMessageEvent, CacheRetention, ContentBlock, Message, Model,
    StopReason, StreamOptions, Tool, TranscriptContext,
};

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Anthropic 兼容字段(02 文档 §3 AnthropicMessagesCompat 的 M1 子集;pi 默认值)。
#[derive(Debug, Clone)]
struct AnthropicCompat {
    supports_long_cache_retention: bool,
    supports_cache_control_on_tools: bool,
    supports_temperature: bool,
    allow_empty_signature: bool,
    supports_strict_tools: bool,
    supports_mid_convo_system_messages: bool,
    force_adaptive_thinking: bool,
}

impl AnthropicCompat {
    fn resolve(model: &Model) -> Self {
        let compat = &model.compat;
        let get = |key: &str| compat.as_ref().and_then(|c| c.get(key)).cloned();
        let flag = |key: &str, default: bool| get(key).and_then(|v| v.as_bool()).unwrap_or(default);
        AnthropicCompat {
            supports_long_cache_retention: flag("supportsLongCacheRetention", true),
            supports_cache_control_on_tools: flag("supportsCacheControlOnTools", true),
            supports_temperature: flag("supportsTemperature", true),
            allow_empty_signature: flag("allowEmptySignature", false),
            supports_strict_tools: flag("supportsStrictTools", false),
            supports_mid_convo_system_messages: flag("supportsMidConvoSystemMessages", false),
            force_adaptive_thinking: flag("forceAdaptiveThinking", false),
        }
    }
}

pub(crate) struct AnthropicAdapter {
    client: reqwest::Client,
}

/// 工厂:以上游 trait 类型出厂(方针文档 §2 规则 1)。
pub fn create_anthropic_adapter() -> std::sync::Arc<dyn Provider> {
    std::sync::Arc::new(AnthropicAdapter::new())
}

impl AnthropicAdapter {
    pub(crate) fn new() -> Self {
        AnthropicAdapter {
            client: reqwest::Client::new(),
        }
    }
}

impl Default for AnthropicAdapter {
    fn default() -> Self {
        Self::new()
    }
}

fn cache_control_value(retention: CacheRetention, supports_long: bool) -> Option<Value> {
    match retention {
        CacheRetention::None => None,
        CacheRetention::Short => Some(json!({"type": "ephemeral"})),
        CacheRetention::Long => {
            if supports_long {
                Some(json!({"type": "ephemeral", "ttl": "1h"}))
            } else {
                Some(json!({"type": "ephemeral"}))
            }
        }
    }
}

fn tool_to_api_value(
    tool: &Tool,
    strict_tools: bool,
    cache_control: Option<&Value>,
    is_last: bool,
) -> Value {
    let schema = &tool.parameters;
    let properties = schema
        .get("properties")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let required = schema.get("required").cloned().unwrap_or_else(|| json!([]));
    let mut value = json!({
        "name": tool.name,
        "description": tool.description,
        "input_schema": {
            "type": "object",
            "properties": properties,
            "required": required,
        },
    });
    if strict_tools {
        if let Some(crate::types::ConstrainedSamplingConfig::JsonSchema {
            strict: crate::types::StrictMode::Require,
        }) = tool.constrained_sampling
        {
            value["strict"] = json!(true);
        }
    }
    if is_last {
        if let Some(cc) = cache_control {
            value["cache_control"] = cc.clone();
        }
    }
    value
}

fn content_blocks_to_api(content: &[ContentBlock]) -> Value {
    let has_images = content
        .iter()
        .any(|b| matches!(b, ContentBlock::Image { .. }));
    if !has_images {
        let text: Vec<&str> = content.iter().filter_map(|b| b.as_text()).collect();
        return json!(text.join("\n"));
    }
    let mut blocks: Vec<Value> = Vec::new();
    let mut has_text = false;
    for block in content {
        match block {
            ContentBlock::Text { text, .. } => {
                has_text = true;
                blocks.push(json!({"type": "text", "text": text}));
            }
            ContentBlock::Image { data, mime_type } => {
                blocks.push(json!({
                    "type": "image",
                    "source": {"type": "base64", "media_type": mime_type, "data": data},
                }));
            }
            _ => {}
        }
    }
    if !has_text {
        blocks.insert(0, json!({"type": "text", "text": "(see attached image)"}));
    }
    json!(blocks)
}

/// 把转录消息转成 Anthropic wire 格式(`messages` 字段;system 单独走顶层字段)。
fn convert_messages(
    messages: &[Message],
    cache_control: Option<&Value>,
    allow_empty_signature: bool,
) -> Vec<Value> {
    use Message as M;
    let mut params: Vec<Value> = Vec::new();
    // 中途 system 消息滞留在下一条 assistant 前(Anthropic 要求 tool_result 紧跟 tool_use)
    let mut pending_system: Vec<Value> = Vec::new();

    let mut index = 0;
    while index < messages.len() {
        let msg = &messages[index];
        match msg {
            M::System { .. } => {
                let text = render_system_message_update(msg);
                if !text.is_empty() {
                    pending_system.push(
                        json!({"role": "system", "content": [{"type": "text", "text": text}]}),
                    );
                }
                index += 1;
            }
            M::Developer { content, .. } => {
                // Anthropic API 没有中途 developer/system 角色:降级为 user 文本
                // (请求级就地提醒);与前一条 user 消息合并,避免连续同角色被拒
                let text = content.trim().to_string();
                if !text.is_empty() {
                    let merged = match params.last_mut() {
                        Some(last) if last.get("role").and_then(Value::as_str) == Some("user") => {
                            match last.get_mut("content") {
                                Some(Value::String(prev)) => {
                                    *last.get_mut("content").unwrap() = json!([
                                        {"type": "text", "text": prev.clone()},
                                        {"type": "text", "text": text.clone()},
                                    ]);
                                    true
                                }
                                Some(Value::Array(blocks)) => {
                                    blocks.push(json!({"type": "text", "text": text.clone()}));
                                    true
                                }
                                _ => false,
                            }
                        }
                        _ => false,
                    };
                    if !merged {
                        params.push(json!({"role": "user", "content": text}));
                    }
                }
                index += 1;
            }
            M::User { content, .. } => {
                match content {
                    crate::types::UserContent::Text(text) => {
                        if !text.trim().is_empty() {
                            params.push(json!({"role": "user", "content": text}));
                        }
                    }
                    crate::types::UserContent::Blocks(blocks) => {
                        let api_blocks: Vec<Value> = blocks
                            .iter()
                            .filter_map(|block| match block {
                                ContentBlock::Text { text, .. } => (!text.trim().is_empty())
                                    .then(|| json!({"type": "text", "text": text})),
                                ContentBlock::Image { data, mime_type } => Some(json!({
                                    "type": "image",
                                    "source": {"type": "base64", "media_type": mime_type, "data": data},
                                })),
                                _ => None,
                            })
                            .collect();
                        if !api_blocks.is_empty() {
                            params.push(json!({"role": "user", "content": api_blocks}));
                        }
                    }
                }
                index += 1;
            }
            M::Assistant(assistant) => {
                params.append(&mut pending_system);
                let mut blocks: Vec<Value> = Vec::new();
                for block in &assistant.content {
                    match block {
                        ContentBlock::Text { text, .. } => {
                            if !text.trim().is_empty() {
                                blocks.push(json!({"type": "text", "text": text}));
                            }
                        }
                        ContentBlock::Thinking {
                            thinking,
                            thinking_signature,
                            redacted,
                        } => {
                            if redacted == &Some(true) {
                                // 密文 payload 原样回放
                                blocks.push(json!({"type": "redacted_thinking", "data": thinking_signature.clone().unwrap_or_default()}));
                                continue;
                            }
                            let signature = thinking_signature.as_deref().unwrap_or("").trim();
                            if thinking.trim().is_empty() && signature.is_empty() {
                                continue;
                            }
                            if signature.is_empty() {
                                // 中断流可能缺签名:默认降级为 text;标记模型可保留空签名块
                                if allow_empty_signature {
                                    blocks.push(json!({"type": "thinking", "thinking": thinking, "signature": ""}));
                                } else {
                                    blocks.push(json!({"type": "text", "text": thinking}));
                                }
                            } else {
                                blocks.push(json!({"type": "thinking", "thinking": thinking, "signature": signature}));
                            }
                        }
                        ContentBlock::ToolCall {
                            id,
                            name,
                            arguments,
                        } => {
                            blocks.push(json!({"type": "tool_use", "id": id, "name": name, "input": arguments}));
                        }
                        ContentBlock::Image { .. } => {}
                    }
                }
                if !blocks.is_empty() {
                    params.push(json!({"role": "assistant", "content": blocks}));
                }
                index += 1;
            }
            M::ToolResult { .. } => {
                // 连续 toolResult 合并进一条 user 消息(z.ai Anthropic 端点需要)
                let mut tool_results: Vec<Value> = Vec::new();
                while index < messages.len() {
                    if let M::ToolResult {
                        tool_call_id,
                        content,
                        is_error,
                        ..
                    } = &messages[index]
                    {
                        tool_results.push(json!({
                            "type": "tool_result",
                            "tool_use_id": tool_call_id,
                            "content": content_blocks_to_api(content),
                            "is_error": is_error,
                        }));
                        index += 1;
                    } else {
                        break;
                    }
                }
                params.push(json!({"role": "user", "content": tool_results}));
            }
        }
    }
    params.append(&mut pending_system);

    // 缓存断点:最后一条 user/system 消息的最后一个块
    if let Some(cc) = cache_control {
        if let Some(last) = params.last_mut() {
            let role = last["role"].as_str().unwrap_or("");
            if role == "user" || role == "system" {
                if let Some(blocks) = last["content"].as_array_mut() {
                    if let Some(last_block) = blocks.last_mut() {
                        last_block["cache_control"] = cc.clone();
                    }
                } else if last["content"]
                    .as_str()
                    .map(|s| !s.is_empty())
                    .unwrap_or(false)
                {
                    let text = last["content"].as_str().unwrap_or_default().to_string();
                    last["content"] = json!([{"type": "text", "text": text, "cache_control": cc}]);
                }
            }
        }
    }
    params
}

fn build_request_body(
    model: &Model,
    context: &TranscriptContext,
    opts: &StreamOptions,
    compat: &AnthropicCompat,
    cache_control: Option<&Value>,
) -> Value {
    let initial = get_initial_system_message(&context.messages);
    let system_text = initial.map(get_system_message_text).unwrap_or_default();
    let conversation = if initial.is_some() {
        &context.messages[1..]
    } else {
        &context.messages[..]
    };

    let mut body = json!({
        "model": model.id,
        "messages": convert_messages(conversation, cache_control, compat.allow_empty_signature),
        "max_tokens": effective_max_tokens(opts.max_tokens, model),
        "stream": true,
    });
    if !system_text.is_empty() {
        let mut system_block = json!({"type": "text", "text": system_text});
        if let Some(cc) = cache_control {
            system_block["cache_control"] = cc.clone();
        }
        body["system"] = json!([system_block]);
    }

    // 温度与扩展思考互斥,且部分模型不支持(02 文档 §3)
    if let Some(temperature) = opts.temperature {
        if opts.reasoning.is_none() && compat.supports_temperature {
            body["temperature"] = json!(temperature);
        }
    }

    if model.reasoning {
        if let Some(level) = opts.reasoning {
            if compat.force_adaptive_thinking {
                body["thinking"] = json!({"type": "adaptive"});
                // AnthropicEffort 无 minimal:minimal/low→low,xhigh/max 无映射时→high(pi 语义)
                let effort = match level {
                    crate::types::ThinkingLevel::Minimal | crate::types::ThinkingLevel::Low => {
                        "low".to_string()
                    }
                    crate::types::ThinkingLevel::Medium => "medium".to_string(),
                    crate::types::ThinkingLevel::High => "high".to_string(),
                    crate::types::ThinkingLevel::Xhigh | crate::types::ThinkingLevel::Max => {
                        map_thinking_level(model, level)
                    }
                };
                body["output_config"] = json!({"effort": effort});
            } else {
                // 预算式思考(pi adjustMaxTokensForThinking):先给思考腾出 max_tokens 空间,
                // 再保证回答至少留 1024 token,避免 budget >= max_tokens 的非法请求
                let base = effective_max_tokens(opts.max_tokens, model);
                let budget_raw = match level {
                    crate::types::ThinkingLevel::Minimal => 1024u64,
                    crate::types::ThinkingLevel::Low => 2048,
                    crate::types::ThinkingLevel::Medium => 4096,
                    crate::types::ThinkingLevel::High => 8192,
                    crate::types::ThinkingLevel::Xhigh | crate::types::ThinkingLevel::Max => 16_384,
                };
                let max_tokens = (base + budget_raw).min(model.max_tokens as u64);
                let budget = budget_raw.min(max_tokens.saturating_sub(1024)).max(1);
                body["max_tokens"] = json!(max_tokens);
                body["thinking"] = json!({"type": "enabled", "budget_tokens": budget});
            }
        }
    }

    let tools = get_current_tools(&context.messages);
    if !tools.is_empty() {
        let tool_cc = if compat.supports_cache_control_on_tools {
            cache_control
        } else {
            None
        };
        let converted: Vec<Value> = tools
            .iter()
            .enumerate()
            .map(|(i, tool)| {
                let is_last = i + 1 == tools.len();
                tool_to_api_value(tool, compat.supports_strict_tools, tool_cc, is_last)
            })
            .collect();
        body["tools"] = json!(converted);
    }

    if let Some(session_id) = &opts.session_id {
        if cache_control.is_some() {
            body["metadata"] = json!({"user_id": session_id});
        }
    }
    body
}

/// Anthropic finish reason → 统一 StopReason(pi mapStopReason)。
fn map_stop_reason(raw: &str, stop_details: Option<&Value>) -> (StopReason, Option<String>) {
    match raw {
        "end_turn" => (StopReason::Stop, None),
        "max_tokens" => (StopReason::Length, None),
        "tool_use" => (StopReason::ToolUse, None),
        "refusal" => {
            let explanation = stop_details
                .and_then(|d| d.get("explanation"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| "The model refused to complete the request".into());
            (StopReason::Error, Some(explanation))
        }
        "pause_turn" | "stop_sequence" => (StopReason::Stop, None),
        "sensitive" => (
            StopReason::Error,
            Some("Provider stopped with: sensitive".into()),
        ),
        other => (
            StopReason::Error,
            Some(format!("Unhandled stop reason: {other}")),
        ),
    }
}

/// 事件处理状态:增量累积 assistant 消息并产出统一事件。
struct StreamState {
    output: AssistantMessage,
    /// provider content index → output.content 下标
    index_map: HashMap<u64, usize>,
    /// toolcall 的部分 JSON 累积(按 output.content 下标)
    partial_json: HashMap<usize, String>,
    saw_message_stop: bool,
}

impl StreamState {
    fn new(model: &Model) -> Self {
        StreamState {
            output: AssistantMessage::pending(model),
            index_map: HashMap::new(),
            partial_json: HashMap::new(),
            saw_message_stop: false,
        }
    }

    fn usage_from(&mut self, usage: &Value) {
        let get = |key: &str| usage.get(key).and_then(|v| v.as_u64()).unwrap_or(0);
        self.output.usage.input = get("input_tokens");
        self.output.usage.output = get("output_tokens");
        self.output.usage.cache_read = get("cache_read_input_tokens");
        self.output.usage.cache_write = get("cache_creation_input_tokens");
        self.output.usage.cache_write_1h = usage
            .pointer("/cache_creation/ephemeral_1h_input_tokens")
            .and_then(|v| v.as_u64());
        self.output.usage.total_tokens = self.output.usage.input
            + self.output.usage.output
            + self.output.usage.cache_read
            + self.output.usage.cache_write;
    }

    fn recompute_total(&mut self) {
        self.output.usage.total_tokens = self.output.usage.input
            + self.output.usage.output
            + self.output.usage.cache_read
            + self.output.usage.cache_write;
    }
}

fn handle_sse_event(
    state: &mut StreamState,
    model: &Model,
    event: &crate::sse::SseEvent,
    events_out: &mut Vec<AssistantMessageEvent>,
    raw_event_observer: &Option<crate::types::OnProviderStreamEvent>,
) -> Result<(), String> {
    if event.event.as_deref() == Some("error") {
        return Err(event.data.clone());
    }
    // pi 语义:SSE event 字段只用于识别 error;分发按 JSON 的 type 字段
    let data: Value = parse_json_with_repair(&event.data).map_err(|e| {
        format!(
            "Could not parse Anthropic SSE event: {e}; data={}",
            event.data
        )
    })?;
    // 归一化前的原始 provider 事件观察(T3)
    observe_provider_event(raw_event_observer, &data);
    let name = data.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match name {
        "message_start" => {
            let message = &data["message"];
            state.output.response_id = message
                .get("id")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            if let Some(response_model) = message.get("model").and_then(|v| v.as_str()) {
                if response_model != model.id {
                    state.output.response_model = Some(response_model.to_string());
                }
            }
            if let Some(usage) = message.get("usage") {
                state.usage_from(usage);
                model.calculate_cost(&mut state.output.usage);
            }
        }
        "content_block_start" => {
            let provider_index = data
                .get("index")
                .and_then(|v| v.as_u64())
                .unwrap_or_default();
            let block = &data["content_block"];
            let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let content_index = state.output.content.len();
            match block_type {
                "text" => {
                    let text = block
                        .get("text")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    state.output.content.push(ContentBlock::Text {
                        text,
                        text_signature: None,
                    });
                    state.index_map.insert(provider_index, content_index);
                    events_out.push(AssistantMessageEvent::TextStart { content_index });
                }
                "thinking" => {
                    let thinking = block
                        .get("thinking")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let signature = block
                        .get("signature")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    state.output.content.push(ContentBlock::Thinking {
                        thinking,
                        thinking_signature: Some(signature),
                        redacted: None,
                    });
                    state.index_map.insert(provider_index, content_index);
                    events_out.push(AssistantMessageEvent::ThinkingStart { content_index });
                }
                "redacted_thinking" => {
                    let payload = block
                        .get("data")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    state.output.content.push(ContentBlock::Thinking {
                        thinking: "[Reasoning redacted]".into(),
                        thinking_signature: Some(payload),
                        redacted: Some(true),
                    });
                    state.index_map.insert(provider_index, content_index);
                    events_out.push(AssistantMessageEvent::ThinkingStart { content_index });
                }
                "tool_use" => {
                    let id = block
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let name = block
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    state.output.content.push(ContentBlock::ToolCall {
                        id,
                        name,
                        arguments: block.get("input").cloned().unwrap_or_else(|| json!({})),
                    });
                    state.index_map.insert(provider_index, content_index);
                    state.partial_json.insert(content_index, String::new());
                    events_out.push(AssistantMessageEvent::ToolCallStart { content_index });
                }
                _ => {
                    // 未知块类型(如 fallback)忽略
                    state.index_map.remove(&provider_index);
                }
            }
        }
        "content_block_delta" => {
            let provider_index = data
                .get("index")
                .and_then(|v| v.as_u64())
                .unwrap_or_default();
            let delta = &data["delta"];
            let delta_type = delta.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match delta_type {
                "text_delta" => {
                    let text = delta.get("text").and_then(|v| v.as_str()).unwrap_or("");
                    if let Some(&i) = state.index_map.get(&provider_index) {
                        if let Some(ContentBlock::Text { text: t, .. }) =
                            state.output.content.get_mut(i)
                        {
                            t.push_str(text);
                            events_out.push(AssistantMessageEvent::TextDelta {
                                content_index: i,
                                delta: text.to_string(),
                            });
                        }
                    }
                }
                "thinking_delta" => {
                    let thinking = delta.get("thinking").and_then(|v| v.as_str()).unwrap_or("");
                    if let Some(&i) = state.index_map.get(&provider_index) {
                        if let Some(ContentBlock::Thinking { thinking: t, .. }) =
                            state.output.content.get_mut(i)
                        {
                            t.push_str(thinking);
                            events_out.push(AssistantMessageEvent::ThinkingDelta {
                                content_index: i,
                                delta: thinking.to_string(),
                            });
                        }
                    }
                }
                "signature_delta" => {
                    let signature = delta
                        .get("signature")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if let Some(&i) = state.index_map.get(&provider_index) {
                        if let Some(ContentBlock::Thinking {
                            thinking_signature, ..
                        }) = state.output.content.get_mut(i)
                        {
                            let sig = thinking_signature.get_or_insert_with(String::new);
                            sig.push_str(signature);
                        }
                    }
                }
                "input_json_delta" => {
                    let partial = delta
                        .get("partial_json")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if let Some(&i) = state.index_map.get(&provider_index) {
                        let json = state.partial_json.entry(i).or_default();
                        json.push_str(partial);
                        if let Some(ContentBlock::ToolCall { arguments, .. }) =
                            state.output.content.get_mut(i)
                        {
                            *arguments = parse_streaming_json(Some(json));
                        }
                        events_out.push(AssistantMessageEvent::ToolCallDelta {
                            content_index: i,
                            delta: partial.to_string(),
                        });
                    }
                }
                _ => {}
            }
        }
        "content_block_stop" => {
            let provider_index = data
                .get("index")
                .and_then(|v| v.as_u64())
                .unwrap_or_default();
            let Some(&content_index) = state.index_map.get(&provider_index) else {
                return Ok(());
            };
            match state.output.content.get(content_index).cloned() {
                Some(ContentBlock::Text { text, .. }) => {
                    events_out.push(AssistantMessageEvent::TextEnd {
                        content_index,
                        content: text,
                    });
                }
                Some(ContentBlock::Thinking { thinking, .. }) => {
                    events_out.push(AssistantMessageEvent::ThinkingEnd {
                        content_index,
                        content: thinking,
                    });
                }
                Some(block @ ContentBlock::ToolCall { .. }) => {
                    // 定稿:用修复解析得到权威 arguments
                    let final_args = state
                        .partial_json
                        .get(&content_index)
                        .map(|json| crate::json_parse::parse_streaming_json(Some(json)))
                        .unwrap_or_else(|| json!({}));
                    if let Some(ContentBlock::ToolCall { arguments, .. }) =
                        state.output.content.get_mut(content_index)
                    {
                        *arguments = final_args;
                    }
                    state.partial_json.remove(&content_index);
                    events_out.push(AssistantMessageEvent::ToolCallEnd {
                        content_index,
                        tool_call: state
                            .output
                            .content
                            .get(content_index)
                            .cloned()
                            .unwrap_or(block),
                    });
                }
                _ => {}
            }
        }
        "message_delta" => {
            if let Some(stop_reason) = data.pointer("/delta/stop_reason").and_then(|v| v.as_str()) {
                let (stop, error_message) =
                    map_stop_reason(stop_reason, data.pointer("/delta/stop_details"));
                state.output.raw_stop_reason = Some(stop_reason.to_string());
                state.output.stop_reason = stop;
                if error_message.is_some() {
                    state.output.error_message = error_message;
                }
            }
            if let Some(usage) = data.get("usage") {
                // 字段可能缺失(null):保留 message_start 的值
                let set = |key: &str, target: &mut u64| {
                    if let Some(v) = usage.get(key).and_then(|v| v.as_u64()) {
                        *target = v;
                    }
                };
                set("input_tokens", &mut state.output.usage.input);
                set("output_tokens", &mut state.output.usage.output);
                set(
                    "cache_read_input_tokens",
                    &mut state.output.usage.cache_read,
                );
                set(
                    "cache_creation_input_tokens",
                    &mut state.output.usage.cache_write,
                );
                if let Some(v) = usage
                    .pointer("/cache_creation/ephemeral_1h_input_tokens")
                    .and_then(|v| v.as_u64())
                {
                    state.output.usage.cache_write_1h = Some(v);
                }
                if let Some(v) = usage
                    .pointer("/output_tokens_details/thinking_tokens")
                    .and_then(|v| v.as_u64())
                {
                    state.output.usage.reasoning = Some(v);
                }
                state.recompute_total();
                model.calculate_cost(&mut state.output.usage);
            }
        }
        "message_stop" => {
            state.saw_message_stop = true;
        }
        _ => {}
    }
    Ok(())
}

async fn stream_impl(
    client: reqwest::Client,
    model: Model,
    context: TranscriptContext,
    opts: StreamOptions,
) -> AssistantMessageEventStream {
    Box::pin(async_stream::stream! {
        let compat = AnthropicCompat::resolve(&model);
        let cache_retention = resolve_cache_retention(opts.cache_retention);
        let cache_control = cache_control_value(cache_retention, compat.supports_long_cache_retention);
        let normalized = resolve_transcript(context, compat.supports_mid_convo_system_messages);
        let mut body = build_request_body(&model, &normalized, &opts, &compat, cache_control.as_ref());
        // 请求体观察/替换(T3;panic 吞掉)
        observe_payload(&opts.on_payload, &mut body);

        // 凭据:显式配置(model.apiKey/opts)优先;白名单 provider 缺 env 报错;
        // 自定义 provider 允许无 key(models.json 体系)
        let api_key = match crate::env_keys::resolve_request_credential(
            &model.provider,
            opts.api_key.as_deref().or(model.api_key.as_deref()),
        ) {
            Ok(api_key) => api_key,
            Err(message) => {
                yield setup_error(&model, message);
                return;
            }
        };

        let base = if model.base_url.is_empty() { DEFAULT_BASE_URL } else { trim_base_url(&model.base_url) };
        let url = format!("{base}/v1/messages");

        let mut defaults = vec![
            ("anthropic-version", ANTHROPIC_VERSION.to_string()),
            ("content-type", "application/json".into()),
            ("accept", "text/event-stream".into()),
            ("user-agent", "latent/0.1".into()),
        ];
        if let Some(api_key) = &api_key {
            defaults.insert(0, ("x-api-key", api_key.clone()));
        }
        let headers = crate::adapters::build_header_map(&defaults, &opts.headers);
        let request = client.post(&url).headers(headers).json(&body);
        // 连接/响应头阶段同样可取消:cancel 原本只在响应体读取时检查,
        // 上游迟迟不响应时 abort 会一直卡在 send() 上(Ctrl+C 表现为无效)
        let response = match opts.cancel.as_ref() {
            Some(token) => {
                tokio::select! {
                    response = request.send() => response,
                    _ = token.cancelled() => {
                        yield aborted_error(&model);
                        return;
                    }
                }
            }
            None => request.send().await,
        };
        let response = match response {
            Ok(r) if !r.status().is_success() => {
                let status = r.status();
                let retry_after = parse_retry_after_header(r.headers().get(reqwest::header::RETRY_AFTER));
                let body = r.text().await.unwrap_or_default();
                yield setup_error(&model, http_error_message(status, &body, retry_after));
                return;
            }
            Ok(r) => {
                observe_response(&opts.on_response, r.status(), &url, r.headers());
                r
            }
            Err(err) => {
                yield setup_error(&model, err.to_string());
                return;
            }
        };

        yield AssistantMessageEvent::Start;
        let mut state = StreamState::new(&model);
        let mut line_decoder = LineDecoder::new();
        let mut sse_decoder = SseDecoder::new();
        let mut bytes: ByteStream = Box::pin(response.bytes_stream());
        let mut terminal: Option<AssistantMessageEvent> = None;

        'outer: loop {
            match read_chunk(&mut bytes, opts.cancel.as_ref()).await {
                ReadOutcome::Chunk(chunk) => {
                    if std::env::var("LATENT_DEBUG").is_ok() {
                        eprintln!("chunk head: {:?}", String::from_utf8_lossy(&chunk[..chunk.len().min(120)]));
                    }
                    for line in line_decoder.feed(&chunk) {
                        if let Some(event) = sse_decoder.feed_line(&line) {
                            let mut events_out = Vec::new();
                            if let Err(message) =
                                handle_sse_event(&mut state, &model, &event, &mut events_out, &opts.on_provider_stream_event)
                            {
                                terminal = Some(error_with_partial(
                                    state.output.clone(),
                                    message,
                                ));
                                break 'outer;
                            }
                            for event in events_out {
                                yield event;
                            }
                            if state.saw_message_stop {
                                break 'outer;
                            }
                        }
                    }
                }
                ReadOutcome::Ended => {
                    if let Some(line) = line_decoder.finish() {
                        if let Some(event) = sse_decoder.feed_line(&line) {
                            let mut events_out = Vec::new();
                            if let Err(message) =
                                handle_sse_event(&mut state, &model, &event, &mut events_out, &opts.on_provider_stream_event)
                            {
                                terminal = Some(error_with_partial(
                                    state.output.clone(),
                                    message,
                                ));
                                break;
                            }
                            for event in events_out {
                                yield event;
                            }
                        }
                    }
                    break;
                }
                ReadOutcome::Aborted => {
                    yield aborted_error(&model);
                    return;
                }
                ReadOutcome::Transport(message) => {
                    terminal = Some(error_with_partial(state.output.clone(), message));
                    break;
                }
            }
        }

        if let Some(event) = terminal {
            yield event;
            return;
        }
        if opts.cancel.as_ref().map(|c| c.is_cancelled()).unwrap_or(false) {
            yield aborted_error(&model);
            return;
        }
        // 流早断:message_start 后没有 message_stop(可被重试分类器识别)
        if state.output.stop_reason == StopReason::Pending {
            let message = if state.output.response_id.is_some() {
                "Anthropic stream ended before message_stop".to_string()
            } else {
                "Anthropic stream ended without a stop reason".to_string()
            };
            yield error_with_partial(state.output, message);
            return;
        }
        if state.output.stop_reason == StopReason::Error || state.output.stop_reason == StopReason::Aborted {
            // provider 侧 error 事件:state.output 已携带 stop_reason/error_message
            // 与已流出内容,直接作为终态(不重建,避免丢内容)
            yield AssistantMessageEvent::Error(Box::new(state.output));
            return;
        }
        yield AssistantMessageEvent::Done(Box::new(state.output));
    })
}

#[async_trait]
impl Provider for AnthropicAdapter {
    async fn stream(
        &self,
        model: &Model,
        context: TranscriptContext,
        opts: StreamOptions,
    ) -> AssistantMessageEventStream {
        stream_impl(self.client.clone(), model.clone(), context, opts).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::collapse_system_messages;

    #[test]
    fn maps_stop_reasons() {
        assert_eq!(map_stop_reason("end_turn", None).0, StopReason::Stop);
        assert_eq!(map_stop_reason("max_tokens", None).0, StopReason::Length);
        assert_eq!(map_stop_reason("tool_use", None).0, StopReason::ToolUse);
        assert_eq!(map_stop_reason("pause_turn", None).0, StopReason::Stop);
        let refusal = map_stop_reason("refusal", Some(&json!({"explanation": "no"})));
        assert_eq!(refusal.0, StopReason::Error);
        assert_eq!(refusal.1.as_deref(), Some("no"));
        assert_eq!(map_stop_reason("mystery", None).0, StopReason::Error);
    }

    #[test]
    fn converts_messages_with_signature_fallback() {
        let assistant = AssistantMessage {
            content: vec![
                ContentBlock::Thinking {
                    thinking: "hmm".into(),
                    thinking_signature: None,
                    redacted: None,
                },
                ContentBlock::text("hi"),
            ],
            ..AssistantMessage::pending(&Model::minimal("m", "anthropic-messages", "anthropic"))
        };
        let messages = vec![Message::user_text("q"), Message::assistant(assistant)];
        let out = convert_messages(&messages, None, false);
        assert_eq!(out.len(), 2);
        // 无签名的 thinking 降级为 text
        let blocks = out[1]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "hmm");
        // allowEmptySignature 时保留 thinking 块
        let out = convert_messages(&messages, None, true);
        let blocks = out[1]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "thinking");
        assert_eq!(blocks[0]["signature"], "");
    }

    #[test]
    fn developer_message_degrades_to_user_and_merges() {
        // Anthropic 无中途 developer 角色:降级为 user 文本;与前一条 user
        // 合并避免连续同角色;独立时也是 user 角色
        let messages = vec![
            Message::user_text("q"),
            Message::developer("mode reminder"),
        ];
        let out = convert_messages(&messages, None, false);
        assert_eq!(out.len(), 1, "应合并进前一条 user");
        let blocks = out[0]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "q");
        assert_eq!(blocks[1]["text"], "mode reminder");

        let mut assistant = AssistantMessage::pending(&Model::minimal(
            "m", "anthropic-messages", "anthropic",
        ));
        assistant.content = vec![ContentBlock::text("a")];
        let messages = vec![
            Message::user_text("q"),
            Message::assistant(assistant),
            Message::developer("mode reminder"),
        ];
        let out = convert_messages(&messages, None, false);
        assert_eq!(out.len(), 3);
        assert_eq!(out[2]["role"], "user");
        assert_eq!(out[2]["content"], "mode reminder");
    }

    #[test]
    fn developer_message_at_end_merges_into_tool_result_message() {
        // 模式节每请求追加在数组末尾:工具轮次末尾是 toolResult(user 角色),
        // developer 降级文本并进同一条 user 消息(tool_result 块在前,文本随后),
        // 避免连续 user 被拒
        let messages = vec![
            Message::user_text("q"),
            Message::tool_result("t1", "read", vec![ContentBlock::text("a")], false),
            Message::developer("mode reminder"),
        ];
        let out = convert_messages(&messages, None, false);
        assert_eq!(out.len(), 2, "{out:?}");
        let blocks = out[1]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "tool_result");
        assert_eq!(blocks[1]["type"], "text");
        assert_eq!(blocks[1]["text"], "mode reminder");
    }

    #[test]
    fn merges_consecutive_tool_results() {
        let messages = vec![
            Message::user_text("q"),
            Message::tool_result("t1", "read", vec![ContentBlock::text("a")], false),
            Message::tool_result("t2", "read", vec![ContentBlock::text("b")], true),
        ];
        let out = convert_messages(&messages, None, false);
        assert_eq!(out.len(), 2);
        let blocks = out[1]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "tool_result");
        assert_eq!(blocks[1]["is_error"], true);
    }

    #[test]
    fn collapse_by_default() {
        let model = Model::minimal("m", "anthropic-messages", "anthropic");
        let compat = AnthropicCompat::resolve(&model);
        assert!(!compat.supports_mid_convo_system_messages);
        let ctx = TranscriptContext {
            messages: vec![
                Message::system("base"),
                Message::user_text("q"),
                Message::system("mid"),
            ],
        };
        let normalized = resolve_transcript(ctx, compat.supports_mid_convo_system_messages);
        let collapsed = collapse_system_messages(normalized);
        let body = build_request_body(&model, &collapsed, &StreamOptions::default(), &compat, None);
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
        assert!(body["system"][0]["text"].as_str().unwrap().contains("base"));
    }
}
