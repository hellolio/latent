//! LLM 世界的纯类型(01 文档 §1/§2、02 文档;serde 形态与 pi JSONL 兼容)。
//!
//! 序列化约定:字段 camelCase;role 判别符 `toolResult`、内容块判别符 `toolCall`
//! 与 pi 完全一致,会话格式兼容优先(方针文档 §6)。

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// 内容块(01 文档 §1.2)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ContentBlock {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text_signature: Option<String>,
    },
    Thinking {
        thinking: String,
        /// provider 侧不透明回放数据;redacted 时密文 payload 存于此
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thinking_signature: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        redacted: Option<bool>,
    },
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    #[serde(rename_all = "camelCase")]
    ToolCall {
        id: String,
        name: String,
        arguments: serde_json::Value,
    },
}

impl ContentBlock {
    pub fn text(text: impl Into<String>) -> Self {
        ContentBlock::Text {
            text: text.into(),
            text_signature: None,
        }
    }

    /// 文本块内容(text 块之外返回 None)
    pub fn as_text(&self) -> Option<&str> {
        match self {
            ContentBlock::Text { text, .. } => Some(text),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Usage / StopReason(01 文档 §1.3)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub total: f64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// 仅 Anthropic 报告的 1h 缓存写入拆分(cache_write 的子集)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_1h: Option<u64>,
    /// reasoning token,是 output 的子集
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<u64>,
    pub total_tokens: u64,
    pub cost: Cost,
}

impl Usage {
    pub fn zero() -> Self {
        Usage::default()
    }
}

/// StopReason(01 文档:7 值联合)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StopReason {
    /// 流式进行中(pi 的 partial 消息默认态)
    Pending,
    Stop,
    /// length 防御:拒绝执行该消息全部工具调用(03 文档不变量)
    Length,
    ToolUse,
    Error,
    Aborted,
    Deferred,
}

// ---------------------------------------------------------------------------
// 工具声明(01 文档 §1.4)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolReference {
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StrictMode {
    Prefer,
    Require,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ConstrainedSamplingConfig {
    #[serde(rename_all = "camelCase")]
    JsonSchema { strict: StrictMode },
    #[serde(rename_all = "camelCase")]
    Grammar { variants: BTreeMap<String, String> },
}

/// 工具声明(进转录 system 消息与请求体;01 文档 §1.4)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tool {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// TypeBox/JSON Schema(pi 里即 JSON Schema 对象)
    pub parameters: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub constrained_sampling: Option<ConstrainedSamplingConfig>,
}

impl Tool {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
    ) -> Self {
        Tool {
            name: name.into(),
            description: description.into(),
            parameters,
            constrained_sampling: None,
        }
    }
}

// ---------------------------------------------------------------------------
// 消息(01 文档 §1.1)
// ---------------------------------------------------------------------------

/// user 消息内容:纯文本或块数组(pi 的 string | (Text|Image)[])。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UserContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

/// LLM 世界消息(经 `convert_to_llm` 折叠后的形态,provider 只见此类型)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Message {
    System {
        content: String,
        /// 命名节:字符串=替换,None=删除(pi 的 sections patch;01 文档)
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        sections: BTreeMap<String, Option<String>>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tools_added: Vec<Tool>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tools_removed: Vec<ToolReference>,
        #[serde(default)]
        timestamp: i64,
    },
    User {
        content: UserContent,
        #[serde(default)]
        timestamp: i64,
    },
    Assistant(Box<AssistantMessage>),
    #[serde(rename = "toolResult", rename_all = "camelCase")]
    ToolResult {
        tool_call_id: String,
        #[serde(default)]
        tool_name: String,
        content: Vec<ContentBlock>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<serde_json::Value>,
        #[serde(default)]
        is_error: bool,
        #[serde(default)]
        timestamp: i64,
    },
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl Message {
    pub fn system(text: impl Into<String>) -> Self {
        Message::System {
            content: text.into(),
            sections: BTreeMap::new(),
            tools_added: Vec::new(),
            tools_removed: Vec::new(),
            timestamp: now_ms(),
        }
    }

    pub fn user_text(text: impl Into<String>) -> Self {
        Message::User {
            content: UserContent::Text(text.into()),
            timestamp: now_ms(),
        }
    }

    pub fn tool_result(
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        content: Vec<ContentBlock>,
        is_error: bool,
    ) -> Self {
        Message::ToolResult {
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            content,
            details: None,
            is_error,
            timestamp: now_ms(),
        }
    }

    pub fn assistant(message: AssistantMessage) -> Self {
        Message::Assistant(Box::new(message))
    }
}

// ---------------------------------------------------------------------------
// AssistantMessage
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessage {
    pub content: Vec<ContentBlock>,
    pub api: String,
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_thinking_level: Option<String>,
    pub usage: Usage,
    pub stop_reason: StopReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_stop_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_turn: Option<bool>,
    #[serde(default)]
    pub timestamp: i64,
}

impl AssistantMessage {
    /// 流式起点:pending 态空消息(适配器从它开始累积)。
    pub fn pending(model: &Model) -> Self {
        AssistantMessage {
            content: Vec::new(),
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            response_model: None,
            response_id: None,
            provider_thinking_level: None,
            usage: Usage::zero(),
            stop_reason: StopReason::Pending,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: now_ms(),
        }
    }

    /// 失败编码进流:stopReason=error/aborted 的终态消息(02 文档流协议)。
    pub fn error(model: &Model, message: impl Into<String>, aborted: bool) -> Self {
        let mut m = AssistantMessage::pending(model);
        m.stop_reason = if aborted {
            StopReason::Aborted
        } else {
            StopReason::Error
        };
        m.error_message = Some(message.into());
        m
    }

    /// 全部 text 块拼接(UI 展示用;01 文档 contentText)。
    pub fn text_content(&self) -> String {
        let parts: Vec<&str> = self.content.iter().filter_map(|b| b.as_text()).collect();
        parts.join("")
    }

    pub fn has_tool_calls(&self) -> bool {
        self.content
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolCall { .. }))
    }
}

// ---------------------------------------------------------------------------
// 模型与成本(01 文档 §2)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCost {
    /// $/百万 token
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tiers: Option<Vec<ModelCostTier>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCostTier {
    /// 总输入 token 超过该阈值时整单采用本档
    pub input_tokens_above: u64,
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

/// pi thinking 级别(01 文档:ai 侧无 off;02 文档 §2)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ThinkingLevel {
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ThinkingLevel {
    pub const ALL: [ThinkingLevel; 6] = [
        ThinkingLevel::Minimal,
        ThinkingLevel::Low,
        ThinkingLevel::Medium,
        ThinkingLevel::High,
        ThinkingLevel::Xhigh,
        ThinkingLevel::Max,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            ThinkingLevel::Minimal => "minimal",
            ThinkingLevel::Low => "low",
            ThinkingLevel::Medium => "medium",
            ThinkingLevel::High => "high",
            ThinkingLevel::Xhigh => "xhigh",
            ThinkingLevel::Max => "max",
        }
    }
}

/// 聊天模型(pi 的 Model,01 文档 §2;compat 为 api 专属 JSON,由各适配器解释)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// API 适配器名(anthropic-messages / openai-completions / …),开放集经
    /// `Provider` 注册表按字符串查找(09 B5.1)
    pub api: String,
    pub provider: String,
    #[serde(default)]
    pub base_url: String,
    /// 已解析的请求凭据(models.json 的 apiKey 经 env 名优先解析后的结果;
    /// None = 无配置凭据,请求侧回退 env 白名单,白名单外 provider 允许无 key)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default)]
    pub input: Vec<String>,
    #[serde(default)]
    pub cost: ModelCost,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<std::collections::HashMap<String, String>>,
    #[serde(default)]
    pub reasoning: bool,
    /// pi 级别 → provider 侧取值;None 表示该级别不支持
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level_map: Option<BTreeMap<String, Option<String>>>,
    pub context_window: u64,
    pub max_tokens: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampling_params: Option<serde_json::Map<String, serde_json::Value>>,
    /// api 专属兼容字段(02 文档 §3:数据驱动的 provider 差异),由适配器解释
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compat: Option<serde_json::Value>,
}

impl Model {
    /// 测试/骨架用最小模型:零价格、默认窗口。
    pub fn minimal(
        id: impl Into<String>,
        api: impl Into<String>,
        provider: impl Into<String>,
    ) -> Self {
        Model {
            id: id.into(),
            name: String::new(),
            api: api.into(),
            provider: provider.into(),
            base_url: String::new(),
            api_key: None,
            input: vec!["text".into()],
            cost: ModelCost::default(),
            headers: None,
            reasoning: false,
            thinking_level_map: None,
            context_window: 128_000,
            max_tokens: 4_096,
            sampling_params: None,
            compat: None,
        }
    }

    /// pi models.ts calculateCost:分层定价 + Anthropic 1h 缓存写入 2x 计价。
    pub fn calculate_cost(&self, usage: &mut Usage) {
        let input_tokens = usage.input + usage.cache_read + usage.cache_write;
        let mut rates = (
            &self.cost.input,
            &self.cost.output,
            &self.cost.cache_read,
            &self.cost.cache_write,
        );
        let mut matched: i64 = -1;
        if let Some(tiers) = &self.cost.tiers {
            for tier in tiers {
                if input_tokens > tier.input_tokens_above
                    && (tier.input_tokens_above as i64) > matched
                {
                    rates = (
                        &tier.input,
                        &tier.output,
                        &tier.cache_read,
                        &tier.cache_write,
                    );
                    matched = tier.input_tokens_above as i64;
                }
            }
        }
        let long_write = usage.cache_write_1h.unwrap_or(0).min(usage.cache_write);
        let short_write = usage.cache_write - long_write;
        usage.cost.input = rates.0 / 1_000_000.0 * usage.input as f64;
        usage.cost.output = rates.1 / 1_000_000.0 * usage.output as f64;
        usage.cost.cache_read = rates.2 / 1_000_000.0 * usage.cache_read as f64;
        usage.cost.cache_write =
            (rates.3 * short_write as f64 + rates.0 * 2.0 * long_write as f64) / 1_000_000.0;
        usage.cost.total =
            usage.cost.input + usage.cost.output + usage.cost.cache_read + usage.cost.cache_write;
    }
}

// ---------------------------------------------------------------------------
// 请求上下文与流协议(02 文档 §1)
// ---------------------------------------------------------------------------

/// 公开入口接受的请求输入;`normalize_context` 把它折叠成 `TranscriptContext`。
#[derive(Debug, Clone, Default)]
pub struct Context {
    pub system_prompt: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<Tool>,
}

/// provider 只能收到已折叠的转录(pi 用 brand 类型保证;见 09 A2 辅助接缝)。
/// 提示词与工具声明由首条 system 消息承载,唯一构造入口是 `normalize_context`。
#[derive(Debug, Clone)]
pub struct TranscriptContext {
    pub messages: Vec<Message>,
}

impl TranscriptContext {
    /// 仅限循环/注册表等已知已折叠场景使用;一般代码请走 `normalize_context`。
    pub fn from_messages(messages: Vec<Message>) -> Self {
        Self { messages }
    }
}

/// 缓存保持期(02 文档 §4.3)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CacheRetention {
    None,
    #[default]
    Short,
    Long,
}

/// HTTP 响应观察载荷(on_response 回调;02 文档 §4.3)。
#[derive(Debug, Clone)]
pub struct ResponseObservation {
    pub status: u16,
    pub url: String,
    pub headers: std::collections::HashMap<String, String>,
}

/// 请求体观察/替换回调(pi 的 onPayload):装配后适配器在发送前调用,
/// 可就地检查或替换 JSON 请求体。panic 被捕获吞掉,不击穿流。
pub type OnPayload = Arc<dyn Fn(&mut serde_json::Value) + Send + Sync>;
/// HTTP 响应观察回调(pi 的 onResponse)。
pub type OnResponse = Arc<dyn Fn(&ResponseObservation) + Send + Sync>;
/// 原始 provider 事件回调(pi 的 onProviderStreamEvent):归一化前的
/// provider 原生事件 JSON。
pub type OnProviderStreamEvent = Arc<dyn Fn(&serde_json::Value) + Send + Sync>;

/// 异步长请求句柄(01 文档 §2 DeferredHandle):provider 侧后台任务的
/// 可轮询凭据;M1 子集仅承载类型,按需接入适配器。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeferredHandle {
    pub provider: String,
    pub model_id: String,
    pub api: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poll_after_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// 流式请求选项(02 文档 §4.3 的 M1 子集;观察回调为 11 计划 T3 增补)。
#[derive(Clone, Default)]
pub struct StreamOptions {
    pub api_key: Option<String>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u32>,
    /// pi thinking 级别(None=不请求思考)
    pub reasoning: Option<ThinkingLevel>,
    pub cache_retention: Option<CacheRetention>,
    /// session 亲和 / prompt cache
    pub session_id: Option<String>,
    /// 额外请求头,覆盖适配器默认
    pub headers: std::collections::HashMap<String, String>,
    /// 任意透传采样参数(llama.cpp/vLLM/SGLang),合并覆盖 Model.samplingParams
    pub sampling_params: serde_json::Map<String, serde_json::Value>,
    /// 中止令牌:取消后事件流以 aborted 终态收尾(不抛异常)
    pub cancel: Option<CancellationToken>,
    /// 请求体观察/替换(发送前调用;None = 不装配,零开销)
    pub on_payload: Option<OnPayload>,
    /// HTTP 响应观察
    pub on_response: Option<OnResponse>,
    /// 归一化前的原始 provider 事件观察
    pub on_provider_stream_event: Option<OnProviderStreamEvent>,
    /// 异步长请求开关(DeferredHandle 类型已备,适配器接入按需)
    pub deferred: bool,
}

/// 统一流协议(02 文档 §1.2):start 先行、done/error 终态,失败编码进流。
/// 块事件携带 content_index 与增量;终态事件携带完整消息。
#[derive(Debug, Clone)]
pub enum AssistantMessageEvent {
    Start,
    TextStart {
        content_index: usize,
    },
    TextDelta {
        content_index: usize,
        delta: String,
    },
    TextEnd {
        content_index: usize,
        content: String,
    },
    ThinkingStart {
        content_index: usize,
    },
    ThinkingDelta {
        content_index: usize,
        delta: String,
    },
    ThinkingEnd {
        content_index: usize,
        content: String,
    },
    ToolCallStart {
        content_index: usize,
    },
    ToolCallDelta {
        content_index: usize,
        delta: String,
    },
    ToolCallEnd {
        content_index: usize,
        tool_call: ContentBlock,
    },
    /// 终态:正常结束(stop/length/toolUse),携带最终消息
    Done(Box<AssistantMessage>),
    /// 终态:失败编码进流(error/aborted),不抛异常
    Error(Box<AssistantMessage>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// P0 回归:AssistantMessage 不得自带 role 键,internally-tagged enum 补齐。
    #[test]
    fn assistant_message_role_comes_from_enum_tag() {
        let model = Model::minimal("m", "anthropic-messages", "anthropic");
        let mut message = AssistantMessage::pending(&model);
        message.content = vec![ContentBlock::text("hi")];
        message.stop_reason = StopReason::Stop;
        let value = serde_json::to_value(Message::assistant(message.clone())).unwrap();
        assert_eq!(value["role"], "assistant");
        assert!(value
            .as_object()
            .unwrap()
            .iter()
            .all(|(k, _)| k != "AssistantMessage"));

        // roundtrip
        let back: Message = serde_json::from_value(value).unwrap();
        assert_eq!(back, Message::assistant(message));
    }

    #[test]
    fn message_roles_match_pi_jsonl() {
        let tool_result = Message::tool_result("t1", "read", vec![ContentBlock::text("x")], false);
        assert_eq!(
            serde_json::to_value(&tool_result).unwrap()["role"],
            "toolResult"
        );

        let user = Message::user_text("q");
        assert_eq!(serde_json::to_value(&user).unwrap()["role"], "user");

        let tool_call = ContentBlock::ToolCall {
            id: "1".into(),
            name: "read".into(),
            arguments: json!({}),
        };
        assert_eq!(
            serde_json::to_value(&tool_call).unwrap()["type"],
            "toolCall"
        );

        // roundtrip 保持判别符
        let back: Message =
            serde_json::from_value(serde_json::to_value(&tool_result).unwrap()).unwrap();
        assert_eq!(back, tool_result);
    }

    #[test]
    fn calculate_cost_applies_tiers_and_long_cache_write() {
        let mut model = Model::minimal("m", "mock", "mock");
        model.cost = ModelCost {
            input: 3.0,
            output: 15.0,
            cache_read: 0.3,
            cache_write: 3.75,
            tiers: Some(vec![ModelCostTier {
                input_tokens_above: 100,
                input: 1.0,
                output: 5.0,
                cache_read: 0.1,
                cache_write: 1.25,
            }]),
        };
        let mut usage = Usage {
            input: 200,
            output: 10,
            cache_write_1h: Some(20),
            ..Usage::zero()
        };
        usage.cache_write = 50;
        model.calculate_cost(&mut usage);
        // 命中高档:input=200>100 → 整单 1.0/5.0 档
        assert!((usage.cost.input - (1.0 / 1e6 * 200.0)).abs() < 1e-12);
        assert!((usage.cost.output - (5.0 / 1e6 * 10.0)).abs() < 1e-12);
        // 1h 写入按 2x input 计价:(1.25*30 + 1.0*2*20)/1e6
        assert!((usage.cost.cache_write - (1.25 * 30.0 + 1.0 * 2.0 * 20.0) / 1e6).abs() < 1e-12);
    }
}
