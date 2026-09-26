//! 事件与订阅者(09 B2):`Vec<Arc<dyn Subscriber>>` 串行 await、按订阅顺序保序,
//! 不用 broadcast channel(无序且不背压)。
//!
//! 事件集合对齐 pi 的 10 种 AgentEvent(01 文档 §3.3):
//! `message_update` 仅 assistant 流式;`tool_execution_end` 并行模式下
//! 按完成序,tool result 消息按源序(03 文档不变量 I4)。
//!
//! 流式增量(T2,03 §10.6 待定点②):**delta 为主 + 快照读口** ——
//! MessageDelta 携带类型化增量(thinking/toolCall 参数),`message_start`
//! 携带 `Arc<RwLock<AssistantMessage>>` 读口,循环在每个 delta 上原地更新
//! partial,UI 随帧读取"到目前为止"的状态;终态快照仍由 message_update /
//! message_end 权威定稿,增量不改变终态内容。

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, RwLock};

use rpi_ai::AssistantMessage;

use crate::message::AgentMessage;

/// "到目前为止"的 partial 消息读口:循环在流式期间原地更新,UI 随帧读取。
/// 不随每个 delta 携带整份快照(热路径分配,11 计划注意事项 5)。
pub type SharedPartial = Arc<RwLock<AssistantMessage>>;

/// 流式增量载荷(T2):文本 / thinking / toolCall 参数增量。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MessageDeltaPayload {
    Text { delta: String },
    Thinking { delta: String },
    ToolCallArgs { content_index: usize, delta: String },
}

impl MessageDeltaPayload {
    pub fn text(delta: impl Into<String>) -> Self {
        MessageDeltaPayload::Text { delta: delta.into() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    /// run 开始
    AgentStart,
    /// 每个 turn 开始
    TurnStart,
    /// assistant 消息 + 全部工具结果之后
    TurnEnd {
        message: Box<AssistantMessage>,
        tool_results: Vec<AgentMessage>,
    },
    /// system/user/assistant/toolResult 消息进入转录;assistant 流式时携带
    /// partial 读口(`serde(skip)`:跨进程由 delta 重建)
    MessageStart {
        message: Box<AgentMessage>,
        /// partial 读口仅进程内有效:serde 跳过(跨进程由 delta 重建)
        #[serde(skip)]
        partial: Option<SharedPartial>,
    },
    /// 流式增量(thinking/toolCall 参数逐块可见;完整快照经 partial 读口取用)
    MessageDelta { delta: MessageDeltaPayload },
    /// assistant 流式:当前 partial 消息快照(终态权威定稿)
    MessageUpdate { message: Box<AssistantMessage> },
    /// 消息定稿(全部消息类型)
    MessageEnd { message: Box<AgentMessage> },
    ToolExecutionStart {
        tool_call_id: String,
        tool_name: String,
        args: serde_json::Value,
    },
    ToolExecutionUpdate {
        tool_call_id: String,
        tool_name: String,
        partial: String,
    },
    /// 工具结算(并行模式下按完成序)
    ToolExecutionEnd {
        tool_call_id: String,
        tool_name: String,
        output: String,
        is_error: bool,
    },
    /// run 结束(最后事件);messages = 本次 run 新增的全部消息
    AgentEnd { messages: Vec<AgentMessage> },
}

/// 事件汇:core/工具/UI/持久化都是订阅者。实现不得 panic,失败自行记录。
#[async_trait]
pub trait Subscriber: Send + Sync {
    async fn on_event(&self, event: &AgentEvent);
}

pub type SharedSubscriber = Arc<dyn Subscriber>;
