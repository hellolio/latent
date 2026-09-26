//! `openai-completions` 适配器(02 文档 §1;pi api/openai-completions.ts 的 Rust 端口)。
//!
//! 与 pi 的差异(有意裁剪,详见 docs/02 踩坑记录):不做 grammar/custom 工具、
//! reasoning_details 回放与 copilot 动态头;thinkingFormat 实现常用六种
//! (openai/openrouter/deepseek/zai/together/qwen),其余按 openai 风格降级。

use std::collections::HashMap;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::adapters::{
    http_error_message, map_thinking_level, observe_payload, observe_provider_event,
    observe_response, read_chunk, resolve_cache_retention, setup_error, trim_base_url, ReadOutcome,
};
use crate::json_parse::{parse_json_with_repair, parse_streaming_json};
use crate::provider::{AssistantMessageEventStream, Provider};
use crate::sse::{LineDecoder, SseDecoder};
use crate::transcript::{
    get_current_tools, get_initial_system_message, get_system_message_text,
    render_system_message_update, resolve_transcript,
};
use crate::types::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, Message, Model, StopReason,
    StreamOptions, Tool, TranscriptContext, Usage,
};

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// OpenAI Completions 兼容字段(02 文档 §3;camelCase JSON,与 pi compat 一致)。
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct OpenAICompat {
    supports_store: Option<bool>,
    supports_developer_role: Option<bool>,
    supports_reasoning_effort: Option<bool>,
    supports_usage_in_streaming: Option<bool>,
    supports_finish_reason: Option<bool>,
    max_tokens_field: Option<String>,
    requires_tool_result_name: Option<bool>,
    requires_assistant_after_tool_result: Option<bool>,
    requires_thinking_as_text: Option<bool>,
    requires_reasoning_content_on_assistant_messages: Option<bool>,
    thinking_format: Option<String>,
    supports_strict_mode: Option<bool>,
    supports_mid_convo_system_messages: Option<bool>,
    zai_tool_stream: Option<bool>,
    cache_control_format: Option<String>,
}

/// 解析后的兼容配置(检测值 + 显式 compat 覆盖)。
#[derive(Debug, Clone)]
struct ResolvedCompat {
    supports_store: bool,
    supports_developer_role: bool,
    supports_reasoning_effort: bool,
    supports_usage_in_streaming: bool,
    supports_finish_reason: bool,
    max_tokens_field: &'static str,
    requires_tool_result_name: bool,
    requires_assistant_after_tool_result: bool,
    requires_thinking_as_text: bool,
    requires_reasoning_content_on_assistant_messages: bool,
    thinking_format: &'static str,
    supports_strict_mode: bool,
    supports_mid_convo_system_messages: bool,
    zai_tool_stream: bool,
}

/// 从 provider/baseUrl 自动检测(pi detectCompat 的常用子集)。
fn detect_compat(model: &Model) -> ResolvedCompat {
    let provider = model.provider.as_str();
    let base_url = model.base_url.to_lowercase();

    let is_zai = matches!(provider, "zai" | "zai-coding-cn")
        || base_url.contains("api.z.ai")
        || base_url.contains("open.bigmodel.cn");
    let is_together = provider == "together"
        || base_url.contains("api.together.ai")
        || base_url.contains("api.together.xyz");
    let is_moonshot = matches!(provider, "moonshotai" | "moonshotai-cn") || base_url.contains("api.moonshot.");
    let is_openrouter = provider == "openrouter" || base_url.contains("openrouter.ai");
    let is_cloudflare = base_url.contains("api.cloudflare.com") || base_url.contains("gateway.ai.cloudflare.com");
    let is_nvidia = provider == "nvidia" || base_url.contains("integrate.api.nvidia.com");
    let is_ant_ling = provider == "ant-ling" || base_url.contains("api.ant-ling.com");
    let is_cerebras = provider == "cerebras" || base_url.contains("cerebras.ai");
    let is_deepseek = provider == "deepseek" || base_url.contains("deepseek.com");
    let is_xai = provider == "xai" || base_url.contains("api.x.ai");

    let is_non_standard = is_nvidia
        || is_cerebras
        || is_xai
        || is_together
        || base_url.contains("chutes.ai")
        || is_deepseek
        || is_zai
        || is_moonshot
        || provider == "opencode"
        || base_url.contains("opencode.ai")
        || is_cloudflare
        || is_ant_ling;

    let use_max_tokens = base_url.contains("chutes.ai")
        || is_deepseek
        || is_moonshot
        || is_cloudflare
        || is_together
        || is_nvidia
        || is_ant_ling
        || is_zai;

    let thinking_format = if is_deepseek {
        "deepseek"
    } else if is_zai {
        "zai"
    } else if is_together {
        "together"
    } else if is_ant_ling {
        // ant-ling 形态未实现(M1 降级为 openai 风格,见模块头注释)
        "openai"
    } else if is_openrouter {
        "openrouter"
    } else {
        "openai"
    };

    ResolvedCompat {
        supports_store: !is_non_standard,
        supports_developer_role: is_openrouter || !is_non_standard,
        supports_reasoning_effort: !is_xai && !is_zai && !is_moonshot && !is_together && !is_cloudflare && !is_nvidia && !is_ant_ling,
        supports_usage_in_streaming: true,
        supports_finish_reason: true,
        max_tokens_field: if use_max_tokens { "max_tokens" } else { "max_completion_tokens" },
        requires_tool_result_name: false,
        requires_assistant_after_tool_result: false,
        requires_thinking_as_text: false,
        requires_reasoning_content_on_assistant_messages: is_deepseek,
        thinking_format,
        supports_strict_mode: false,
        supports_mid_convo_system_messages: false,
        zai_tool_stream: false,
    }
}

fn resolve_compat(model: &Model) -> ResolvedCompat {
    let detected = detect_compat(model);
    let Some(compat) = &model.compat else { return detected };
    let parsed: OpenAICompat = serde_json::from_value(compat.clone()).unwrap_or_default();
    let opt_bool = |value: Option<bool>, detected: bool| value.unwrap_or(detected);
    ResolvedCompat {
        supports_store: opt_bool(parsed.supports_store, detected.supports_store),
        supports_developer_role: opt_bool(parsed.supports_developer_role, detected.supports_developer_role),
        supports_reasoning_effort: opt_bool(parsed.supports_reasoning_effort, detected.supports_reasoning_effort),
        supports_usage_in_streaming: opt_bool(parsed.supports_usage_in_streaming, detected.supports_usage_in_streaming),
        supports_finish_reason: opt_bool(parsed.supports_finish_reason, detected.supports_finish_reason),
        max_tokens_field: match parsed.max_tokens_field.as_deref() {
            Some("max_tokens") => "max_tokens",
            Some("max_completion_tokens") => "max_completion_tokens",
            _ => detected.max_tokens_field,
        },
        requires_tool_result_name: opt_bool(parsed.requires_tool_result_name, detected.requires_tool_result_name),
        requires_assistant_after_tool_result: opt_bool(
            parsed.requires_assistant_after_tool_result,
            detected.requires_assistant_after_tool_result,
        ),
        requires_thinking_as_text: opt_bool(parsed.requires_thinking_as_text, detected.requires_thinking_as_text),
        requires_reasoning_content_on_assistant_messages: opt_bool(
            parsed.requires_reasoning_content_on_assistant_messages,
            detected.requires_reasoning_content_on_assistant_messages,
        ),
        thinking_format: match parsed.thinking_format.as_deref() {
            Some("openrouter") => "openrouter",
            Some("deepseek") => "deepseek",
            Some("zai") => "zai",
            Some("together") => "together",
            Some("qwen") => "qwen",
            _ => detected.thinking_format,
        },
        supports_strict_mode: opt_bool(parsed.supports_strict_mode, detected.supports_strict_mode),
        supports_mid_convo_system_messages: opt_bool(
            parsed.supports_mid_convo_system_messages,
            detected.supports_mid_convo_system_messages,
        ),
        zai_tool_stream: opt_bool(parsed.zai_tool_stream, detected.zai_tool_stream),
    }
}

fn tool_to_api_value(tool: &Tool, compat: &ResolvedCompat) -> Value {
    let mut function = json!({
        "name": tool.name,
        "description": tool.description,
        "parameters": tool.parameters,
    });
    if compat.supports_strict_mode {
        let strict = matches!(
            tool.constrained_sampling,
            Some(crate::types::ConstrainedSamplingConfig::JsonSchema { .. })
        );
        function["strict"] = json!(strict);
    }
    json!({"type": "function", "function": function})
}

fn convert_messages(model: &Model, messages: &[Message], compat: &ResolvedCompat) -> Vec<Value> {
    let mut params: Vec<Value> = Vec::new();
    let instruction_role = if model.reasoning && compat.supports_developer_role { "developer" } else { "system" };
    let mut last_role: Option<&str> = None;

    let mut index = 0;
    while index < messages.len() {
        let msg = &messages[index];
        match msg {
            Message::System { .. } => {
                let text = if get_initial_system_message(messages).map(|m| std::ptr::eq(m, msg)).unwrap_or(false)
                {
                    get_system_message_text(msg)
                } else {
                    render_system_message_update(msg)
                };
                if !text.is_empty() {
                    params.push(json!({"role": instruction_role, "content": text}));
                }
                last_role = Some("system");
                index += 1;
            }
            Message::User { content, .. } => {
                // 部分 provider 不允许 user 紧跟 tool 结果:插入合成 assistant 桥接
                if compat.requires_assistant_after_tool_result && last_role == Some("toolResult") {
                    params.push(json!({"role": "assistant", "content": "I have processed the tool results."}));
                }
                match content {
                    crate::types::UserContent::Text(text) => {
                        if !text.is_empty() {
                            params.push(json!({"role": "user", "content": text}));
                        }
                    }
                    crate::types::UserContent::Blocks(blocks) => {
                        let parts: Vec<Value> = blocks
                            .iter()
                            .filter_map(|block| match block {
                                ContentBlock::Text { text, .. } => (!text.is_empty())
                                    .then(|| json!({"type": "text", "text": text})),
                                ContentBlock::Image { data, mime_type } => Some(json!({
                                    "type": "image_url",
                                    "image_url": {"url": format!("data:{mime_type};base64,{data}")},
                                })),
                                _ => None,
                            })
                            .collect();
                        if !parts.is_empty() {
                            params.push(json!({"role": "user", "content": parts}));
                        }
                    }
                }
                last_role = Some("user");
                index += 1;
            }
            Message::Assistant(assistant) => {
                let mut assistant_msg = Map::new();
                assistant_msg.insert("role".into(), json!("assistant"));
                assistant_msg.insert("content".into(), Value::Null);

                let text_parts: Vec<String> = assistant
                    .content
                    .iter()
                    .filter_map(|b| b.as_text().filter(|t| !t.trim().is_empty()))
                    .map(str::to_string)
                    .collect();
                let assistant_text = text_parts.join("");
                let thinking_blocks: Vec<&ContentBlock> = assistant
                    .content
                    .iter()
                    .filter(|b| matches!(b, ContentBlock::Thinking { .. }))
                    .collect();
                let tool_calls: Vec<&ContentBlock> = assistant
                    .content
                    .iter()
                    .filter(|b| matches!(b, ContentBlock::ToolCall { .. }))
                    .collect();

                let non_empty_thinking: Vec<&ContentBlock> = thinking_blocks
                    .iter()
                    .copied()
                    .filter(|b| match b {
                        ContentBlock::Thinking { thinking, .. } => !thinking.trim().is_empty(),
                        _ => false,
                    })
                    .collect();
                if !non_empty_thinking.is_empty() {
                    if compat.requires_thinking_as_text {
                        let joined: String = non_empty_thinking
                            .iter()
                            .map(|b| match b {
                                ContentBlock::Thinking { thinking, .. } => thinking.clone(),
                                _ => String::new(),
                            })
                            .collect::<Vec<_>>()
                            .join("\n\n");
                        assistant_msg["content"] = json!([{ "type": "text", "text": joined }, {
                            "type": "text", "text": assistant_text,
                        }]);
                    } else {
                        if !assistant_text.is_empty() {
                            assistant_msg["content"] = json!(assistant_text);
                        }
                        // 思考内容按签名里记录的字段名回放(reasoning_content / reasoning / reasoning_text)
                        if let Some(ContentBlock::Thinking { thinking: _, thinking_signature, .. }) =
                            non_empty_thinking.first().copied()
                        {
                            let field = thinking_signature.as_deref().unwrap_or("reasoning_content");
                            let field = if matches!(field, "reasoning_content" | "reasoning" | "reasoning_text") {
                                field
                            } else {
                                "reasoning_content"
                            };
                            let joined = non_empty_thinking
                                .iter()
                                .map(|b| match b {
                                    ContentBlock::Thinking { thinking, .. } => thinking.clone(),
                                    _ => String::new(),
                                })
                                .collect::<Vec<_>>()
                                .join("\n");
                            assistant_msg.insert(field.to_string(), json!(joined));
                        }
                    }
                } else if !assistant_text.is_empty() {
                    assistant_msg["content"] = json!(assistant_text);
                }

                if !tool_calls.is_empty() {
                    let calls: Vec<Value> = tool_calls
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::ToolCall { id, name, arguments } => Some(json!({
                                "id": id,
                                "type": "function",
                                "function": {"name": name, "arguments": serde_json::to_string(arguments).unwrap_or_else(|_| "{}".into())},
                            })),
                            _ => None,
                        })
                        .collect();
                    assistant_msg.insert("tool_calls".into(), json!(calls));
                }
                if compat.requires_reasoning_content_on_assistant_messages
                    && model.reasoning
                    && !assistant_msg.contains_key("reasoning_content")
                {
                    assistant_msg.insert("reasoning_content".into(), json!(""));
                }
                // 空 assistant 消息(中断流)直接跳过:部分 provider 拒绝"无 content 且无 tool_calls"
                let has_content = assistant_msg["content"].as_str().map(|s| !s.is_empty()).unwrap_or(false)
                    || assistant_msg["content"].as_array().map(|a| !a.is_empty()).unwrap_or(false);
                if has_content || assistant_msg.contains_key("tool_calls") {
                    params.push(Value::Object(assistant_msg));
                }
                last_role = Some("assistant");
                index += 1;
            }
            Message::ToolResult { .. } => {
                while index < messages.len() {
                    if let Message::ToolResult { tool_call_id, tool_name, content, .. } = &messages[index] {
                        let text: Vec<&str> = content.iter().filter_map(|b| b.as_text()).collect();
                        let text = text.join("\n");
                        let has_text = !text.is_empty();
                        let tool_text = if has_text {
                            text
                        } else {
                            "(no tool output)".to_string()
                        };
                        let mut tool_msg = json!({
                            "role": "tool",
                            "content": tool_text,
                            "tool_call_id": tool_call_id,
                        });
                        if compat.requires_tool_result_name && !tool_name.is_empty() {
                            tool_msg["name"] = json!(tool_name);
                        }
                        params.push(tool_msg);
                        index += 1;
                    } else {
                        break;
                    }
                }
                last_role = Some("toolResult");
            }
        }
    }
    params
}

fn build_request_body(
    model: &Model,
    context: &TranscriptContext,
    opts: &StreamOptions,
    compat: &ResolvedCompat,
) -> Value {
    let mut body = json!({
        "model": model.id,
        "messages": convert_messages(model, &context.messages, compat),
        "stream": true,
    });
    if compat.supports_usage_in_streaming {
        body["stream_options"] = json!({"include_usage": true});
    }
    if compat.supports_store {
        body["store"] = json!(false);
    }
    let max_tokens = opts.max_tokens.unwrap_or(model.max_tokens);
    body[compat.max_tokens_field] = json!(max_tokens);
    if let Some(temperature) = opts.temperature {
        body["temperature"] = json!(temperature);
    }

    let tools = get_current_tools(&context.messages);
    if !tools.is_empty() {
        let converted: Vec<Value> = tools.iter().map(|t| tool_to_api_value(t, compat)).collect();
        body["tools"] = json!(converted);
        if compat.zai_tool_stream {
            body["tool_stream"] = json!(true);
        }
    }

    // 思考参数按 thinkingFormat 数据化投放(02 文档 §3)
    if model.reasoning {
        let effort = opts.reasoning.map(|level| map_thinking_level(model, level));
        let supports_effort = compat.supports_reasoning_effort;
        match compat.thinking_format {
            "deepseek" => {
                if effort.is_some() {
                    body["thinking"] = json!({"type": "enabled"});
                } else {
                    body["thinking"] = json!({"type": "disabled"});
                }
                if let (Some(e), true) = (&effort, supports_effort) {
                    body["reasoning_effort"] = json!(e);
                }
            }
            "zai" => {
                body["thinking"] = if effort.is_some() {
                    json!({"type": "enabled", "clear_thinking": false})
                } else {
                    json!({"type": "disabled"})
                };
                if let (Some(e), true) = (&effort, supports_effort) {
                    body["reasoning_effort"] = json!(e);
                }
            }
            "openrouter" => {
                body["reasoning"] = match &effort {
                    Some(e) => json!({"effort": e}),
                    None => json!({"effort": off_thinking_value(model).unwrap_or_else(|| "none".into())}),
                };
            }
            "together" => {
                body["reasoning"] = json!({"enabled": effort.is_some()});
                if let (Some(e), true) = (&effort, supports_effort) {
                    body["reasoning_effort"] = json!(e);
                }
            }
            "qwen" => {
                body["enable_thinking"] = json!(effort.is_some());
                if let (Some(e), true) = (&effort, supports_effort) {
                    body["reasoning_effort"] = json!(e);
                }
            }
            // openai 及未实现的变体:openai 风格 reasoning_effort;off 态回落映射表
            _ => {
                if let (Some(e), true) = (&effort, supports_effort) {
                    body["reasoning_effort"] = json!(e);
                } else if effort.is_none() && supports_effort {
                    if let Some(off) = off_thinking_value(model) {
                        body["reasoning_effort"] = json!(off);
                    }
                }
            }
        }
    }

    // 任意透传采样参数:模型默认在前,请求级覆盖在后(02 文档 §4.3)
    if let Some(sampling) = &model.sampling_params {
        for (key, value) in sampling {
            body[key.as_str()] = value.clone();
        }
    }
    for (key, value) in &opts.sampling_params {
        body[key.as_str()] = value.clone();
    }
    body
}

/// thinkingLevelMap 里 off 级别的 provider 侧取值(pi 的 thinkingLevelMap?.off)。
fn off_thinking_value(model: &Model) -> Option<String> {
    model
        .thinking_level_map
        .as_ref()?
        .get("off")?
        .clone()
}

fn map_stop_reason(raw: &str) -> (StopReason, Option<String>) {
    match raw {
        "stop" | "end" => (StopReason::Stop, None),
        "length" => (StopReason::Length, None),
        "function_call" | "tool_calls" => (StopReason::ToolUse, None),
        "content_filter" => (StopReason::Error, Some("Provider finish_reason: content_filter".into())),
        "network_error" => (StopReason::Error, Some("Provider finish_reason: network_error".into())),
        other => (StopReason::Error, Some(format!("Provider finish_reason: {other}"))),
    }
}

fn parse_chunk_usage(raw: &Value, model: &Model) -> Usage {
    let get = |key: &str| raw.get(key).and_then(|v| v.as_u64()).unwrap_or(0);
    let prompt_tokens = get("prompt_tokens");
    let cache_read = raw
        .pointer("/prompt_tokens_details/cached_tokens")
        .and_then(|v| v.as_u64())
        .or_else(|| raw.get("prompt_cache_hit_tokens").and_then(|v| v.as_u64()))
        .or_else(|| raw.get("cached_tokens").and_then(|v| v.as_u64()))
        .unwrap_or(0);
    let cache_write = raw
        .pointer("/prompt_tokens_details/cache_write_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let reasoning = raw
        .pointer("/completion_tokens_details/reasoning_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let input = prompt_tokens.saturating_sub(cache_read + cache_write);
    let output = get("completion_tokens");
    let mut usage = Usage {
        input,
        output,
        cache_read,
        cache_write,
        reasoning: Some(reasoning),
        total_tokens: input + output + cache_read + cache_write,
        ..Usage::zero()
    };
    model.calculate_cost(&mut usage);
    usage
}

/// 流式块处理状态(OpenAI 无 per-block index,text/thinking 各一块)。
struct StreamState {
    output: AssistantMessage,
    text_index: Option<usize>,
    thinking_index: Option<usize>,
    /// OpenAI 流内 toolcall index → output.content 下标
    tool_by_stream_index: HashMap<u64, usize>,
    /// toolcall id → output.content 下标
    tool_by_id: HashMap<String, usize>,
    partial_args: HashMap<usize, String>,
    has_finish_reason: bool,
}

impl StreamState {
    fn new(model: &Model) -> Self {
        StreamState {
            output: AssistantMessage::pending(model),
            text_index: None,
            thinking_index: None,
            tool_by_stream_index: HashMap::new(),
            tool_by_id: HashMap::new(),
            partial_args: HashMap::new(),
            has_finish_reason: false,
        }
    }
}

fn ensure_text_block(state: &mut StreamState, events_out: &mut Vec<AssistantMessageEvent>) -> usize {
    if let Some(i) = state.text_index {
        return i;
    }
    let content_index = state.output.content.len();
    state.output.content.push(ContentBlock::Text { text: String::new(), text_signature: None });
    state.text_index = Some(content_index);
    events_out.push(AssistantMessageEvent::TextStart { content_index });
    content_index
}

fn ensure_thinking_block(state: &mut StreamState, signature: &str, events_out: &mut Vec<AssistantMessageEvent>) -> usize {
    if let Some(i) = state.thinking_index {
        return i;
    }
    let content_index = state.output.content.len();
    state.output.content.push(ContentBlock::Thinking {
        thinking: String::new(),
        thinking_signature: Some(signature.to_string()),
        redacted: None,
    });
    state.thinking_index = Some(content_index);
    events_out.push(AssistantMessageEvent::ThinkingStart { content_index });
    content_index
}

fn ensure_tool_call_block(
    state: &mut StreamState,
    stream_index: Option<u64>,
    id: &str,
    name: &str,
    events_out: &mut Vec<AssistantMessageEvent>,
) -> usize {
    if let Some(idx) = stream_index.and_then(|i| state.tool_by_stream_index.get(&i)) {
        return *idx;
    }
    if let Some(idx) = state.tool_by_id.get(id) {
        return *idx;
    }
    let content_index = state.output.content.len();
    state.output.content.push(ContentBlock::ToolCall {
        id: id.to_string(),
        name: name.to_string(),
        arguments: json!({}),
    });
    state.partial_args.insert(content_index, String::new());
    if let Some(i) = stream_index {
        state.tool_by_stream_index.insert(i, content_index);
    }
    if !id.is_empty() {
        state.tool_by_id.insert(id.to_string(), content_index);
    }
    events_out.push(AssistantMessageEvent::ToolCallStart { content_index });
    content_index
}

fn handle_chunk(
    state: &mut StreamState,
    model: &Model,
    chunk: &Value,
    events_out: &mut Vec<AssistantMessageEvent>,
) {
    if state.output.response_id.is_none() {
        if let Some(id) = chunk.get("id").and_then(|v| v.as_str()) {
            state.output.response_id = Some(id.to_string());
        }
    }
    if state.output.response_model.is_none() {
        if let Some(response_model) = chunk.get("model").and_then(|v| v.as_str()) {
            if response_model != model.id {
                state.output.response_model = Some(response_model.to_string());
            }
        }
    }
    if let Some(usage) = chunk.get("usage").filter(|u| u.is_object()) {
        state.output.usage = parse_chunk_usage(usage, model);
    }
    // 回退:部分 provider(Moonshot)把 usage 放在 choice.usage
    let choice = chunk.get("choices").and_then(|c| c.as_array()).and_then(|c| c.first());
    let Some(choice) = choice else { return };
    if chunk.get("usage").map(|u| !u.is_object()).unwrap_or(true) {
        if let Some(usage) = choice.get("usage").filter(|u| u.is_object()) {
            state.output.usage = parse_chunk_usage(usage, model);
        }
    }

    if let Some(finish_reason) = choice.get("finish_reason") {
        // finish_reason 可能是 null(未结束)
        if let Some(raw) = finish_reason.as_str() {
            state.output.raw_stop_reason = Some(raw.to_string());
            let (stop, error_message) = map_stop_reason(raw);
            state.output.stop_reason = stop;
            if error_message.is_some() {
                state.output.error_message = error_message;
            }
            state.has_finish_reason = true;
        }
    }

    let Some(delta) = choice.get("delta") else { return };

    if let Some(content) = delta.get("content").and_then(|v| v.as_str()) {
        if !content.is_empty() {
            let content_index = ensure_text_block(state, events_out);
            if let Some(ContentBlock::Text { text, .. }) = state.output.content.get_mut(content_index) {
                text.push_str(content);
            }
            events_out.push(AssistantMessageEvent::TextDelta {
                content_index,
                delta: content.to_string(),
            });
        }
    }

    // reasoning_content(llama.cpp)/ reasoning(通用兼容)/ reasoning_text,取首个非空
    for field in ["reasoning_content", "reasoning", "reasoning_text"] {
        if let Some(reasoning) = delta.get(field).and_then(|v| v.as_str()) {
            if !reasoning.is_empty() {
                let content_index = ensure_thinking_block(state, field, events_out);
                if let Some(ContentBlock::Thinking { thinking, .. }) = state.output.content.get_mut(content_index) {
                    thinking.push_str(reasoning);
                }
                events_out.push(AssistantMessageEvent::ThinkingDelta {
                    content_index,
                    delta: reasoning.to_string(),
                });
                break;
            }
        }
    }

    if let Some(tool_calls) = delta.get("tool_calls").and_then(|v| v.as_array()) {
        for call in tool_calls {
            let stream_index = call.get("index").and_then(|v| v.as_u64());
            let id = call.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let name = call
                .pointer("/function/name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let content_index = ensure_tool_call_block(state, stream_index, id, name, events_out);
            if !id.is_empty() {
                if let Some(ContentBlock::ToolCall { id: block_id, .. }) = state.output.content.get_mut(content_index)
                {
                    if block_id.is_empty() {
                        *block_id = id.to_string();
                    }
                }
                state.tool_by_id.insert(id.to_string(), content_index);
            }
            if !name.is_empty() {
                if let Some(ContentBlock::ToolCall { name: block_name, .. }) =
                    state.output.content.get_mut(content_index)
                {
                    if block_name.is_empty() {
                        *block_name = name.to_string();
                    }
                }
            }
            if let Some(arguments) = call.pointer("/function/arguments").and_then(|v| v.as_str()) {
                let json = state.partial_args.entry(content_index).or_default();
                json.push_str(arguments);
                if let Some(ContentBlock::ToolCall { arguments, .. }) = state.output.content.get_mut(content_index) {
                    *arguments = parse_streaming_json(Some(json));
                }
                events_out.push(AssistantMessageEvent::ToolCallDelta {
                    content_index,
                    delta: arguments.to_string(),
                });
            }
        }
    }
}

/// 结束所有未关闭的块(text/thinking/toolcall 的 *_end 事件)。
fn finish_blocks(state: &mut StreamState, events_out: &mut Vec<AssistantMessageEvent>) {
    // 按块定稿顺序输出 end
    for content_index in 0..state.output.content.len() {
        match state.output.content.get(content_index).cloned() {
            Some(ContentBlock::Text { text, .. }) => {
                events_out.push(AssistantMessageEvent::TextEnd { content_index, content: text });
            }
            Some(block @ ContentBlock::ToolCall { .. }) => {
                let final_args = state
                    .partial_args
                    .get(&content_index)
                    .map(|json| crate::json_parse::parse_streaming_json(Some(json)))
                    .unwrap_or_else(|| json!({}));
                if let Some(ContentBlock::ToolCall { arguments, .. }) = state.output.content.get_mut(content_index) {
                    *arguments = final_args;
                }
                state.partial_args.remove(&content_index);
                events_out.push(AssistantMessageEvent::ToolCallEnd {
                    content_index,
                    tool_call: state.output.content.get(content_index).cloned().unwrap_or(block),
                });
            }
            Some(ContentBlock::Thinking { thinking, .. }) => {
                events_out.push(AssistantMessageEvent::ThinkingEnd { content_index, content: thinking });
            }
            Some(ContentBlock::Image { .. }) => {}
            None => break,
        }
    }
}

async fn stream_impl(
    client: reqwest::Client,
    model: Model,
    context: TranscriptContext,
    opts: StreamOptions,
) -> AssistantMessageEventStream {
    Box::pin(async_stream::stream! {
        let compat = resolve_compat(&model);
        let _ = resolve_cache_retention(opts.cache_retention); // 保留 env 兼容行为;openai 侧 cacheRetention 由 compat 决定
        let normalized = resolve_transcript(context, compat.supports_mid_convo_system_messages);
        let mut body = build_request_body(&model, &normalized, &opts, &compat);
        // 请求体观察/替换(T3;panic 吞掉)
        observe_payload(&opts.on_payload, &mut body);

        let Some(api_key) = crate::env_keys::resolve_api_key(&model.provider, opts.api_key.as_deref()) else {
            yield setup_error(&model, format!("No API key for provider: {}", model.provider));
            return;
        };

        let base = if model.base_url.is_empty() { DEFAULT_BASE_URL } else { trim_base_url(&model.base_url) };
        let url = format!("{base}/chat/completions");

        let headers = crate::adapters::build_header_map(
            &[
                ("authorization", format!("Bearer {api_key}")),
                ("content-type", "application/json".into()),
                ("accept", "text/event-stream".into()),
                ("user-agent", "rpi/0.1".into()),
            ],
            &opts.headers,
        );
        let response = client
            .post(&url)
            .headers(headers)
            .json(&body)
            .send()
            .await;
        let response = match response {
            Ok(r) if !r.status().is_success() => {
                let status = r.status();
                let body = r.text().await.unwrap_or_default();
                yield setup_error(&model, http_error_message(status, &body));
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
        let mut bytes: crate::adapters::ByteStream = Box::pin(response.bytes_stream());
        let mut terminal: Option<AssistantMessageEvent> = None;

        'outer: loop {
            match read_chunk(&mut bytes, opts.cancel.as_ref()).await {
                ReadOutcome::Chunk(chunk) => {
                    for line in line_decoder.feed(&chunk) {
                        if let Some(event) = sse_decoder.feed_line(&line) {
                            // [DONE] 哨兵
                            if event.data.trim() == "[DONE]" {
                                break 'outer;
                            }
                            match parse_json_with_repair(&event.data) {
                                Ok(chunk_value) => {
                                    // 归一化前的原始 provider 事件观察(T3)
                                    observe_provider_event(&opts.on_provider_stream_event, &chunk_value);
                                    let mut events_out = Vec::new();
                                    handle_chunk(&mut state, &model, &chunk_value, &mut events_out);
                                    for event in events_out {
                                        yield event;
                                    }
                                }
                                Err(err) => {
                                    terminal = Some(AssistantMessageEvent::Error(Box::new(
                                        AssistantMessage::error(
                                            &model,
                                            format!("Could not parse OpenAI SSE chunk: {err}; data={}", event.data),
                                            false,
                                        ),
                                    )));
                                    break 'outer;
                                }
                            }
                        }
                    }
                }
                ReadOutcome::Ended => {
                    if let Some(line) = line_decoder.finish() {
                        if let Some(event) = sse_decoder.feed_line(&line) {
                            if event.data.trim() != "[DONE]" {
                                if let Ok(chunk_value) = parse_json_with_repair(&event.data) {
                                    observe_provider_event(&opts.on_provider_stream_event, &chunk_value);
                                    let mut events_out = Vec::new();
                                    handle_chunk(&mut state, &model, &chunk_value, &mut events_out);
                                    for event in events_out {
                                        yield event;
                                    }
                                }
                            }
                        }
                    }
                    break;
                }
                ReadOutcome::Aborted => {
                    yield crate::adapters::aborted_error(&model);
                    return;
                }
                ReadOutcome::Transport(message) => {
                    terminal = Some(AssistantMessageEvent::Error(Box::new(
                        AssistantMessage::error(&model, message, false),
                    )));
                    break;
                }
            }
        }

        if let Some(event) = terminal {
            yield event;
            return;
        }

        let mut final_events = Vec::new();
        finish_blocks(&mut state, &mut final_events);
        for event in final_events {
            yield event;
        }

        if opts.cancel.as_ref().map(|c| c.is_cancelled()).unwrap_or(false) {
            yield crate::adapters::aborted_error(&model);
            return;
        }
        // finish_reason 缺失时的推断与校验(pi 语义)
        if !state.has_finish_reason {
            if !compat.supports_finish_reason {
                state.output.stop_reason = if state.output.has_tool_calls() {
                    StopReason::ToolUse
                } else {
                    StopReason::Stop
                };
            } else {
                yield AssistantMessageEvent::Error(Box::new(AssistantMessage::error(
                    &model,
                    "Stream ended without finish_reason",
                    false,
                )));
                return;
            }
        }
        if state.output.stop_reason == StopReason::Pending {
            yield AssistantMessageEvent::Error(Box::new(AssistantMessage::error(
                &model,
                "Stream ended without finish_reason",
                false,
            )));
            return;
        }
        if state.output.stop_reason == StopReason::Error {
            let message = state.output.error_message.clone().unwrap_or_else(|| "provider error".into());
            yield AssistantMessageEvent::Error(Box::new(AssistantMessage::error(&model, message, false)));
            return;
        }
        yield AssistantMessageEvent::Done(Box::new(state.output));
    })
}

#[async_trait]
impl Provider for OpenAICompletionsAdapter {
    async fn stream(
        &self,
        model: &Model,
        context: TranscriptContext,
        opts: StreamOptions,
    ) -> AssistantMessageEventStream {
        stream_impl(self.client.clone(), model.clone(), context, opts).await
    }
}

pub(crate) struct OpenAICompletionsAdapter {
    client: reqwest::Client,
}

/// 工厂:以上游 trait 类型出厂(方针文档 §2 规则 1)。
pub fn create_openai_completions_adapter() -> std::sync::Arc<dyn Provider> {
    std::sync::Arc::new(OpenAICompletionsAdapter::new())
}

impl OpenAICompletionsAdapter {
    pub(crate) fn new() -> Self {
        OpenAICompletionsAdapter { client: reqwest::Client::new() }
    }
}

impl Default for OpenAICompletionsAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_finish_reasons() {
        assert_eq!(map_stop_reason("stop").0, StopReason::Stop);
        assert_eq!(map_stop_reason("length").0, StopReason::Length);
        assert_eq!(map_stop_reason("tool_calls").0, StopReason::ToolUse);
        assert_eq!(map_stop_reason("content_filter").0, StopReason::Error);
        assert_eq!(map_stop_reason("weird").0, StopReason::Error);
    }

    #[test]
    fn parses_usage_with_cache_split() {
        let model = Model::minimal("m", "openai-completions", "openai");
        let usage = parse_chunk_usage(
            &json!({
                "prompt_tokens": 100,
                "completion_tokens": 50,
                "prompt_tokens_details": {"cached_tokens": 30, "cache_write_tokens": 10},
                "completion_tokens_details": {"reasoning_tokens": 20},
            }),
            &model,
        );
        assert_eq!(usage.input, 60);
        assert_eq!(usage.cache_read, 30);
        assert_eq!(usage.cache_write, 10);
        assert_eq!(usage.output, 50);
        assert_eq!(usage.reasoning, Some(20));
        assert_eq!(usage.total_tokens, 150);
    }

    #[test]
    fn detects_deepseek_compat() {
        let mut model = Model::minimal("deepseek-chat", "openai-completions", "deepseek");
        model.base_url = "https://api.deepseek.com".into();
        let compat = resolve_compat(&model);
        assert_eq!(compat.thinking_format, "deepseek");
        assert_eq!(compat.max_tokens_field, "max_tokens");
        assert!(compat.requires_reasoning_content_on_assistant_messages);
        // 显式 compat 覆盖
        model.compat = Some(json!({"thinkingFormat": "openrouter", "maxTokensField": "max_completion_tokens"}));
        let compat = resolve_compat(&model);
        assert_eq!(compat.thinking_format, "openrouter");
        assert_eq!(compat.max_tokens_field, "max_completion_tokens");
        // 覆盖不改变未提及字段
        assert!(compat.requires_reasoning_content_on_assistant_messages);
    }

    #[test]
    fn converts_tool_results_and_calls() {
        let model = Model::minimal("m", "openai-completions", "openai");
        let assistant = AssistantMessage {
            content: vec![ContentBlock::ToolCall {
                id: "call_1".into(),
                name: "read".into(),
                arguments: json!({"path": "a.txt"}),
            }],
            ..AssistantMessage::pending(&model)
        };
        let messages = vec![
            Message::user_text("q"),
            Message::assistant(assistant),
            Message::tool_result("call_1", "read", vec![ContentBlock::text("file data")], false),
        ];
        let compat = resolve_compat(&model);
        let out = convert_messages(&model, &messages, &compat);
        assert_eq!(out.len(), 3);
        assert_eq!(out[1]["tool_calls"][0]["function"]["name"], "read");
        assert_eq!(out[2]["role"], "tool");
        assert_eq!(out[2]["tool_call_id"], "call_1");
    }

    #[test]
    fn thinking_replays_into_reasoning_content() {
        let model = Model::minimal("m", "openai-completions", "openai");
        let assistant = AssistantMessage {
            content: vec![
                ContentBlock::Thinking {
                    thinking: "deep thought".into(),
                    thinking_signature: Some("reasoning_content".into()),
                    redacted: None,
                },
                ContentBlock::text("answer"),
            ],
            ..AssistantMessage::pending(&model)
        };
        let messages = vec![Message::assistant(assistant)];
        let compat = resolve_compat(&model);
        let out = convert_messages(&model, &messages, &compat);
        assert_eq!(out[0]["reasoning_content"], "deep thought");
        assert_eq!(out[0]["content"], "answer");
        // 空 assistant(中断流)被跳过
        let empty = AssistantMessage::pending(&model);
        let out = convert_messages(&model, &[Message::assistant(empty)], &compat);
        assert!(out.is_empty());
    }
}
